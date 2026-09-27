//! `BashTool` — runs a shell command via M2-04 sandbox dispatch + M2-06
//! process polish.
//!
//! See `docs/superpowers/plans/2026-05-24-m4-02-shell.md` for the task list
//! and `docs/superpowers/specs/2026-05-24-m4-tools-implementation-design.md`
//! §4 Flow C and §7 wire identifiers for the locked literals.
//!
//! Image-output handling (claude-code `formatOutput` / `isImageOutput`,
//! `BashTool/utils.ts`): a base64 `data:image/...;base64,...` stdout is
//! returned as an IMAGE content block (riding on `new_messages` per the Rust
//! image contract, mirroring FileRead) rather than truncated as text — see the
//! short-circuit in `call`. `resizeShellImageOutput` (CC-304 image
//! dimension/byte cap) reuses the same image budget processor as FileRead
//! before the data URI reaches the model.
//!
//! Timeout/interrupt handling (claude-code `BashTool.tsx` ~602-605 / 720):
//! a timed-out (or interrupted) command is surfaced as a SUCCESSFUL
//! `tool_result` carrying `interrupted: true`, `timed_out: true`, and
//! whatever partial stdout/stderr was captured — NOT a hard error. The
//! `<error>Command was aborted before completion</error>` marker is appended
//! to stderr and `is_error` follows `interrupted`. See `interrupted_result`.

//! ## CLI-5 `bashEditDiffEnabled` — the contract, and what is built
//!
//! 2.1.269 shows a diff of the files a Bash command changed. The machinery
//! lives in [`crate::bash_edit_diff`]; this is the contract it implements,
//! read at the 2.1.270 oracle on 2026-09-14.
//!
//! ⚠️ RESIDUAL, deliberately out: the changed-file list is not handed to
//! `PostToolUse` Bash hooks in `tool_response`. That is a hook-payload change,
//! not part of the tool result, and it has no consumer here yet.
//!
//! ### The gate — `y0r(mode)`
//!
//! ```js
//! if (env.CLAUDE_CODE_BASH_EDIT_DIFF !== undefined) return env.CLAUDE_CODE_BASH_EDIT_DIFF;
//! let tier = Bk("bashEditDiffEnabled")[0];      // USER / FLAG / POLICY tiers only
//! if (tier === false || Ge().bashEditDiffEnabled === false) return false;
//! if (tier === true) return true;
//! return (mode === "auto" || mode === "bypassPermissions") && Aot();
//! ```
//!
//! The tier split is the security-relevant half: only user, flag or policy
//! settings may turn it ON outside `auto`/`bypassPermissions`, so a checked-in
//! project settings file cannot make Claude start reading and diffing the
//! repo's files. `EffectiveSettings::effective_for(field)` exposes the
//! contributing layers, so the tier is reachable here.
//!
//! 🚨 `Aot()` is `CLAUDE_CODE_THRIFTY_SONIC` else a statsig cohort flag
//! defaulting FALSE — so the "on by default in auto mode" arm is DEAD in an
//! unconfigured install, exactly like MEM-4's `tengu_onyx_plover`. A faithful
//! port is opt-in, which also keeps the blast radius small.
//!
//! ### The snapshot — `x0r(workTree, gitDir)`, and why it is safe
//!
//! It never touches the user's repository. Every git call runs with
//! `GIT_DIR=<shadow>` / `GIT_WORK_TREE=<real worktree>`, where the shadow is a
//! private git directory whose `objects/info/alternates` points at the real
//! object store. Its own index, its own `refs/heads/main` (written by
//! `C0r` via `commit-tree` so GC cannot drop the snapshot). ⛔ Running
//! `update-index` against the real `.git` would stage the user's whole tree —
//! the shadow is not an optimisation, it is the safety property.
//!
//! 1. delete a stale `<shadow>/index.lock`;
//! 2. `git status --porcelain=v2 -z --untracked-files=normal --ignored=no
//!    --no-renames --ignore-submodules=all` with `GIT_OPTIONAL_LOCKS=1`;
//! 3. untracked DIRECTORIES expand through `git ls-files -z --others
//!    --exclude-standard --`, batched under 32000 bytes of pathspec;
//! 4. a file over 20_000_000 bytes, or unreadable, is NOT hashed: it goes to a
//!    side ledger as `"{size} {mtimeMs}"` and only bumps a changed counter;
//! 5. `update-index --force-remove -z --stdin` for deletions, then
//!    `update-index --add --remove -z --stdin` for changes;
//! 6. `write-tree`, then `commit-tree` to pin it.
//!
//! Timeouts 750 ms (2000 on Windows) warm / 10 s cold; two consecutive
//! failures for a repo disable the feature for that repo.
//!
//! ### The diff — `b0r(snapshot)`
//!
//! `git diff-tree -r --raw -p --no-color --no-renames <before> <after>`, then:
//!
//! * raw lines `^:\d{6} \d{6} (\S+) (\S+) ([A-Z])\t(.+)$` give
//!   `(srcSha, dstSha, status, path)`;
//! * 🚨 `c0r(sha)` is NOT a binary check — it tests whether the sha is the
//!   EMPTY BLOB (`e69de29…` SHA-1 / `473a0f4…` SHA-256). `A` + empty dst means
//!   "created empty", `D` + empty src "deleted an empty file"; those render as
//!   created/deleted with NO hunks;
//! * a file is skipped when its hunks total ≥ 400 lines, and a file is kept
//!   only while total lines ≤ 400 AND total chars ≤ 64000;
//! * at most 5 files carry hunks; at most 200 paths are listed as changed;
//!   `moreFiles` reports the remainder.
//!
//! Identical before/after trees return `null` — no diff section at all.
//!
//! ### Per call — `Sn = y0r(mode) && !run_in_background && !SNt(input)`
//!
//! A backgrounded command has no "after" yet; a command permission classifies
//! as READ-ONLY cannot have changed anything; and the snapshot additionally
//! requires `!_0r(command)` — a bare `git checkout`/`switch`/… would diff the
//! branch, not the command. See `edit_diff_eligible` in `call`.
//!
//! ⚠️ The MODE half of the gate is resolved once, at the composition root,
//! rather than per call as `y0r(s.permissions().mode)` is upstream. Every other
//! input to the gate (env, tiers, merged value, rollout flag) is fixed for a
//! session, so the only divergence is a mid-session mode switch INTO `auto` /
//! `bypassPermissions` — and that arm needs `tengu_thrifty_sonic`, which
//! defaults false upstream too, so it is unreachable in a stock install.

use crate::shared::strip_ansi_count;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::result::{PermissionPrompt, SandboxOverrideReason};
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::process::ProcessOutputFile;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    BASH_COMPLETED, BASH_EXPLICITLY_BACKGROUNDED, BASH_FAILED, BASH_STARTED, BASH_TIMEOUT,
    BASH_TIMEOUT_BACKGROUNDED,
};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH;
use tool_api::BuiltinToolContext;

// ===== Locked constants =====================================================

/// 2-minute default Bash timeout — the fallback when `BASH_DEFAULT_TIMEOUT_MS`
/// is unset (see [`bash_default_timeout_ms`]).
pub const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 10-minute default maximum Bash timeout — the fallback floor when
/// `BASH_MAX_TIMEOUT_MS` is unset (see [`bash_max_timeout_ms`]).
pub const BASH_MAX_TIMEOUT_MS: u64 = 600_000;
/// Byte-locked timeout error template. `{N}` is substituted at call time.
pub const BASH_TIMEOUT_ERROR_TEMPLATE: &str = "Bash command timed out after {N}ms";

/// claude-code `s0e()` — the effective default Bash timeout. Reads
/// `BASH_DEFAULT_TIMEOUT_MS` via [`resolve_default_timeout_ms`].
#[must_use]
pub fn bash_default_timeout_ms() -> u64 {
    resolve_default_timeout_ms(std::env::var("BASH_DEFAULT_TIMEOUT_MS").ok().as_deref())
}

/// Pure resolver for `s0e()`: `parseInt(raw,10)` used iff `> 0`, else
/// [`BASH_DEFAULT_TIMEOUT_MS`] (120000).
#[must_use]
pub fn resolve_default_timeout_ms(raw: Option<&str>) -> u64 {
    raw.and_then(parse_int_js)
        .filter(|&n| n > 0)
        .map(|n| n as u64)
        .unwrap_or(BASH_DEFAULT_TIMEOUT_MS)
}

/// claude-code `j3n()` — the effective maximum Bash timeout (advisory; shown in
/// the schema `describe` text and the system prompt). Reads `BASH_MAX_TIMEOUT_MS`
/// via [`resolve_max_timeout_ms`].
#[must_use]
pub fn bash_max_timeout_ms() -> u64 {
    resolve_max_timeout_ms(
        std::env::var("BASH_MAX_TIMEOUT_MS").ok().as_deref(),
        bash_default_timeout_ms(),
    )
}

/// Pure resolver for `j3n()`: `parseInt(raw,10)` when `> 0` → `max(n, default)`;
/// otherwise `max(`[`BASH_MAX_TIMEOUT_MS`]` (600000), default)`. The floor
/// against `default` mirrors the binary's `Math.max(_, s0e(env))`.
#[must_use]
pub fn resolve_max_timeout_ms(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(parse_int_js)
        .filter(|&n| n > 0)
        .map(|n| (n as u64).max(default))
        .unwrap_or_else(|| BASH_MAX_TIMEOUT_MS.max(default))
}
/// Tool name byte-lock — matches claude-code tool registry.
pub const TOOL_NAME: &str = "Bash";

/// Format the locked timeout error string with `{N}` substituted.
#[must_use]
pub fn format_timeout_error(timeout_ms: u64) -> String {
    BASH_TIMEOUT_ERROR_TEMPLATE.replace("{N}", &timeout_ms.to_string())
}

/// Minimum sleep duration (seconds) that triggers the sleep-block — byte-locked
/// to claude-code `G2n=25`. Sleeps shorter than this are permitted (the
/// `if(o<G2n)return null` guard in `t2p`); LingXi previously used 2.
pub const SLEEP_BLOCK_THRESHOLD_SECS: f64 = 25.0;

/// Whether the bash sleep-block is active — the LingXi analogue of claude-code's
/// `sq()=ct("tengu_amber_sentinel", false)` gate. Defaults to **false** (so the
/// block is inert in the default config, exactly like stock claude-code), and is
/// opt-in via a truthy `tengu_amber_sentinel` env override (1/true/yes/on). This
/// mirrors `is_agent_swarms_enabled`'s env-based gate accessor (the GrowthBook
/// gate is a host-runtime signal not threaded into the tool, so LingXi defaults
/// it and exposes an env override for parity/testing).
#[must_use]
pub fn sleep_block_enabled() -> bool {
    std::env::var("tengu_amber_sentinel").ok().is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Format a parsed sleep duration the way JS `${parseFloat(x)}` would: an
/// integral value renders WITHOUT a decimal point (`30`, not `30.0`), a
/// fractional value keeps its fraction (`30.5`).
#[must_use]
fn format_parsefloat(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Detect a standalone or leading `sleep N` (N>=25) pattern that should use the
/// background path / Monitor tool instead of blocking the turn. Faithful port
/// of claude-code `t2p` (`BashTool` `detectBlockedSleepPattern`): splits on the
/// shell list separators, then matches `^sleep\s+(\d+(?:\.\d*)?)\s*$` on the
/// FIRST subcommand only, `parseFloat`s the capture, and returns `null` when it
/// is `< G2n` ([`SLEEP_BLOCK_THRESHOLD_SECS`] = 25). FRACTIONAL durations ARE
/// matched (`sleep 30.5`), unlike the prior integer-only port. The duration is
/// rendered with `parseFloat` semantics. Returns the pattern description
/// (`standalone sleep N` or `sleep N followed by: <rest>`), else `None`.
#[must_use]
pub fn detect_blocked_sleep_pattern(command: &str) -> Option<String> {
    let parts = permission::shell_command::split_command(command);
    let first = parts.first().map(|s| s.trim()).unwrap_or("");
    // `^sleep\s+(\d+(?:\.\d*)?)\s*$` — manual match (no regex dep).
    let rest_after_kw = first.strip_prefix("sleep")?;
    // Require at least one whitespace char after the keyword.
    let after_ws = rest_after_kw.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if after_ws.len() == rest_after_kw.len() {
        return None; // no separating whitespace (e.g. "sleeper")
    }
    // Consume the digit run, then an OPTIONAL `.` + further (optional) digits.
    let bytes = after_ws.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None; // `\d+` needs at least one leading digit
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    let (num_str, tail) = after_ws.split_at(i);
    if !tail
        .trim_end_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return None;
    }
    // `parseFloat`: a trailing `.` (e.g. `30.`) is fine in JS but not for Rust's
    // f64 parser, so strip it first.
    let secs: f64 = num_str.trim_end_matches('.').parse().ok()?;
    if secs < SLEEP_BLOCK_THRESHOLD_SECS {
        return None; // sub-threshold sleeps are fine (rate limiting, pacing)
    }
    let rest = parts
        .iter()
        .skip(1)
        .map(|s| s.trim())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    let num_fmt = format_parsefloat(secs);
    if rest.is_empty() {
        Some(format!("standalone sleep {num_fmt}"))
    } else {
        Some(format!("sleep {num_fmt} followed by: {rest}"))
    }
}

/// `^[-+]?\d+(\.\d+)?$` — claude-code `VF`'s numeric-string predicate (manual,
/// no regex dep). NOTE the fractional part requires ≥1 digit after the `.`
/// (`\.\d+`), unlike the sleep regex's `\.\d*`.
#[must_use]
fn is_vf_numeric_string(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == int_start {
        return false; // need ≥1 integer digit
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return false; // `.` must be followed by ≥1 digit
        }
    }
    i == b.len()
}

/// Resolve the `timeout` input to milliseconds — 1:1 with claude-code's `VF`
/// preprocess (numeric-string coercion) followed by `H5a` (`typeof n==="number"
/// && n>0 ? n : default`). A string matching [`is_vf_numeric_string`] is coerced
/// to a number; the value is used iff it is a finite number > 0, else the
/// default. There is NO upper clamp/rejection (the `max` in the schema's
/// `describe` text is advisory only — claude-code's schema has no `.max()`).
#[must_use]
pub fn resolve_timeout_ms(input: &Value) -> u64 {
    let as_num: Option<f64> = match input.get("timeout") {
        Some(Value::String(s)) => {
            let t = s.trim();
            if is_vf_numeric_string(t) {
                t.parse::<f64>().ok().filter(|n| n.is_finite())
            } else {
                None
            }
        }
        Some(v) => v.as_f64(),
        None => None,
    };
    match as_num {
        Some(n) if n > 0.0 => n as u64,
        _ => bash_default_timeout_ms(),
    }
}

// Shared with Monitor; reexport preserves the established shell-tool API.
#[cfg(test)]
use platform_api::shell_support::git_bash_beside_git;
pub use platform_api::shell_support::{
    classify_git_bash_override, format_duration_ms, git_bash_override_warning, git_bash_path,
    resolve_git_bash_path_with, resolve_shell_path, GitBashOverride, BASH_SHELL_LINUX,
    BASH_SHELL_MACOS,
};

// ===== BASH.3 — output-length env override ==================================

/// claude-code `outputLimits.ts` `BASH_MAX_OUTPUT_DEFAULT`.
pub const BASH_MAX_OUTPUT_DEFAULT: usize = 30_000;
/// claude-code `outputLimits.ts` `BASH_MAX_OUTPUT_UPPER_LIMIT`.
pub const BASH_MAX_OUTPUT_UPPER_LIMIT: usize = 150_000;

/// `parseInt(value, 10)` semantics: skip leading ASCII whitespace, an optional
/// sign, then consume leading ASCII digits. Returns `None` (JS `NaN`) when no
/// digit is found. Trailing non-digits are ignored (`"123abc"` ⇒ `123`).
fn parse_int_js(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let mut sign: i64 = 1;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        if b[i] == b'-' {
            sign = -1;
        }
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    // Overflow (astronomically long digit run) ⇒ saturate so the upper-limit
    // cap below still applies; JS would yield a huge float here.
    match s[start..i].parse::<i64>() {
        Ok(v) => Some(sign * v),
        Err(_) => Some(sign * i64::MAX),
    }
}

/// Resolve the effective Bash output cap from a raw `BASH_MAX_OUTPUT_LENGTH`
/// value. 1:1 port of claude-code `envValidation.ts` `validateBoundedIntEnvVar`
/// (driven by `outputLimits.ts` `getMaxOutputLength`): unset/empty/`NaN`/`<= 0`
/// falls back to the default; values above the upper limit are capped.
#[must_use]
pub fn resolve_max_output_length(raw: Option<&str>) -> usize {
    let Some(value) = raw.filter(|v| !v.is_empty()) else {
        return BASH_MAX_OUTPUT_DEFAULT;
    };
    match parse_int_js(value) {
        Some(parsed) if parsed > 0 => {
            if parsed > BASH_MAX_OUTPUT_UPPER_LIMIT as i64 {
                BASH_MAX_OUTPUT_UPPER_LIMIT
            } else {
                parsed as usize
            }
        }
        _ => BASH_MAX_OUTPUT_DEFAULT,
    }
}

/// The `settings.bashOutputMaxChars` clamp — claude-code `see()`:
/// `Math.min(Math.max(e, 4000), 128000)`, or `None` when the setting is absent.
///
/// A value outside the range is PULLED to the nearest bound, not rejected —
/// the same rule `taskOutputMaxChars` uses, and what makes the setting safe to
/// honour ahead of the env var.
fn settings_bash_output_cap() -> Option<usize> {
    platform_api::session_flags::bash_output_max_chars()
        .map(|v| (v as usize).clamp(BASH_OUTPUT_SETTING_MIN, BASH_OUTPUT_SETTING_MAX))
}

/// Lower bound of the `see()` clamp (2.1.261).
pub const BASH_OUTPUT_SETTING_MIN: usize = 4_000;
/// Upper bound of the `see()` clamp — the CHANGELOG's "up to 128K characters".
pub const BASH_OUTPUT_SETTING_MAX: usize = 128_000;

/// Resolve the effective Bash output cap.
///
/// Precedence (2.1.261, matching `taskOutputMaxChars`):
/// 1. `settings.bashOutputMaxChars`, clamped to `4_000..=128_000`;
/// 2. otherwise `BASH_MAX_OUTPUT_LENGTH` over [`BASH_MAX_OUTPUT_DEFAULT`].
///
/// The setting WINS over the env var — it is range-clamped, so it cannot be
/// used to set a pathological cap the env var would have to guard against.
#[must_use]
pub fn bash_max_output_length() -> usize {
    if let Some(from_settings) = settings_bash_output_cap() {
        return from_settings;
    }
    resolve_max_output_length(std::env::var("BASH_MAX_OUTPUT_LENGTH").ok().as_deref())
}

/// Port of claude-code `TFo` (`function TFo(){return
/// st(process.env.LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR)}`): when truthy, the
/// shell cwd is ALWAYS reset to the original (workspace) after a command, even
/// for an in-workspace `cd`. `st` is the strict env-truthy allowlist
/// (`1`/`true`/`yes`/`on`) — delegated to the canonical [`platform_api::env::is_env_truthy`]
/// so it cannot drift. DEFAULT FALSE (unset/empty ⇒ false).
fn tfo_maintain_cwd() -> bool {
    platform_api::env::is_env_truthy(
        std::env::var("LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR")
            .ok()
            .as_deref(),
    )
}

/// Apply claude-code `R0`'s `/private/var` → `/var` and `/private/tmp` → `/tmp`
/// path normalization (the macOS realpath-prefix folding) to a single path.
/// 1:1 with the two `.replace(...)` calls in `R0`:
///   `.replace(/^\/private\/var\//,"/var/").replace(/^\/private\/tmp(\/|$)/,"/tmp$1")`
/// Returns the normalized string form (lossless for our containment compare).
fn normalize_private_prefix(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("/private/var/") {
        return format!("/var/{rest}");
    }
    // `/private/tmp(\/|$)` — the `$1` keeps the trailing `/` (or end-of-string).
    if let Some(rest) = s.strip_prefix("/private/tmp/") {
        return format!("/tmp/{rest}");
    }
    if s == "/private/tmp" {
        return "/tmp".to_string();
    }
    s.into_owned()
}

/// Port of claude-code `R0(e, t)` (`caseFold:false`): "is path `e` contained in
/// directory `t`?" — the predicate `kF` applies over the allowed-dir set.
///
/// `R0` realpaths both via `Ds`, applies the `/private/var`+`/private/tmp`
/// normalization to BOTH, then computes `posix.relative(t, e)`:
///   - `""`            (same path)            ⇒ contained          → true
///   - contains `..`   (`poe`)                ⇒ outside            → false
///   - absolute        (no common base)       ⇒ outside            → false
///   - else (relative, non-`..`)              ⇒ inside             → true
///
/// `cwd` arrives already canonicalized (the readback `canon`); we canonicalize
/// `dir` to mirror `Ds`'s realpath, then fold the `/private/*` prefixes on both
/// before the prefix-containment check. caseFold is FALSE on the `kF` path
/// (case-sensitive) so we compare bytes directly.
fn is_within_allowed(cwd: &std::path::Path, dir: &std::path::Path) -> bool {
    // `Ds` realpaths; `cwd` is already `canon`. Canonicalize `dir`; if that
    // fails (dir gone), fall back to the raw `dir` so the compare still runs
    // (claude-code's `Ds` would throw and `R0`'s callers treat a throw as
    // not-contained, but a missing workspace is already handled upstream by the
    // deleted-cwd recovery — this fallback only affects an exotic race).
    let dir_canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let e = normalize_private_prefix(cwd);
    let t = normalize_private_prefix(&dir_canon);
    if e == t {
        // `posix.relative(t, e)` == "" ⇒ same dir ⇒ contained.
        return true;
    }
    // `posix.relative(t, e)` for `e` strictly inside `t` is a forward relative
    // path with no leading `..` and is not absolute. We model that with a
    // path-component prefix check: `e` starts with `t` AND the boundary is a
    // path separator (so `/tmp/foobar` is NOT "inside" `/tmp/foo`).
    let t_with_sep = if t.ends_with('/') {
        t.clone()
    } else {
        format!("{t}/")
    };
    e.starts_with(&t_with_sep)
}

// no-truncation: A1/STEP-4. Bash no longer truncates its model-facing output.
// claude-code 2.1.220 dropped the shared shell truncator entirely — the only
// `[N lines truncated] ...` sites left in the binary are prompt prose
// (@231867509), the diff renderer (@232474811), and the NOTEBOOK formatter
// `vtd` (@232559907, sole caller `CCs`). Oversized Bash results are handled by
// the orchestrator's `<persisted-output>` layer keyed on
// [`tool_api::tool_trait::Tool::persistence_threshold`]; the REPL/notebook path
// still routes through
// [`tool_api::util::output_truncation::truncate_shell_output`], which is the
// port of `vtd`.

/// The interrupt/abort marker appended to stderr (`BashTool.tsx:602-604`).
const ABORT_MARKER: &str = "<error>Command was aborted before completion</error>";
const SANDBOX_VIOLATIONS_OPEN: &str = "<sandbox_violations>";
const SANDBOX_VIOLATIONS_CLOSE: &str = "</sandbox_violations>";

fn append_sandbox_violations(stderr: &str, violations: &[String]) -> String {
    if violations.is_empty() {
        return stderr.to_string();
    }
    let mut out = stderr.to_string();
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(SANDBOX_VIOLATIONS_OPEN);
    out.push('\n');
    out.push_str(&violations.join("\n"));
    out.push('\n');
    out.push_str(SANDBOX_VIOLATIONS_CLOSE);
    out
}

/// Build the MODEL-facing `tool_result` content string exactly as claude-code's
/// Bash result mapper: `content: [c, u, d].filter(Boolean).join("\n")`
/// (v2.1.185 binary offset 202593080). The model sees this plain-text string —
/// NOT a JSON dump of the structured `data` object (that object still flows to
/// the TUI / `PostToolUse` hook). Parts:
/// - `c` = stdout via `replace(/^(\s*\n)+/,"").trimEnd()` ([`normalize_stdout`]),
/// - `u` = `stderr.trim()`, plus (when `interrupted`) a `\n` separator (`rYa`,
///   only when stderr was non-empty) and the [`ABORT_MARKER`],
/// - `d` = an optional background-run note,
///
/// then the non-empty parts are joined by `\n`.
/// `trailing_note` is the mapper's tail slot. The oracle joins
/// `[stdout, stderr, backgroundNote, staleReadFileStateHint, ghRateLimitHint]`
/// with `\n`; the background note and the stale-read hint are MUTUALLY
/// EXCLUSIVE (the hint is only computed when `!backgroundTaskId`), so one slot
/// reproduces both positions byte-for-byte. `ghRateLimitHint` has no port
/// surface yet.
fn bash_model_content(
    stdout: &str,
    stderr: &str,
    interrupted: bool,
    trailing_note: Option<&str>,
) -> String {
    let c = crate::shared::normalize_stdout(stdout);
    let mut u = stderr.trim().to_string();
    if interrupted {
        if !stderr.is_empty() {
            u.push('\n');
        }
        u.push_str(ABORT_MARKER);
    }
    let mut parts: Vec<String> = Vec::new();
    if !c.is_empty() {
        parts.push(c);
    }
    if !u.is_empty() {
        parts.push(u);
    }
    if let Some(d) = trailing_note {
        if !d.is_empty() {
            parts.push(d.to_string());
        }
    }
    parts.join("\n")
}

/// Build the BashTool result `data` — claude-code 2.1.263 `BashTool` outputSchema
/// (pure metadata; the model-facing render rides on `ToolCallResult.model_content`,
/// NOT inside `data`). Field order mirrors the binary's `return{data:{…}}`
/// construction (`preserve_order` is on): `stdout, stderr, interrupted, isImage,
/// returnCodeInterpretation?, noOutputExpected, backgroundTaskId?,
/// persistedOutputPath?, persistedOutputSize?`. Every `?`-field is
/// omitted when absent, matching the binary's `undefined` values that
/// `JSON.stringify` drops. The
/// telemetry-only fields LingXi used to carry here (`exit_code`, `is_error`,
/// `timed_out`, `truncated`) are NOT part of the result data — they live in the
/// `tengu`/`BASH_COMPLETED` analytics payload only.
///
/// `output_file` is set only when a completed foreground process spilled to its
/// rooted task-output file. 2.1.263 maps that identity onto
/// `persistedOutputPath` / `persistedOutputSize` (the 2.1.191
/// `outputTaskId`/`outputFilePath`/`outputFileSize` trio is gone from the
/// schema). The two fields are emitted together, immediately after
/// `backgroundTaskId` (when present), so a completed auto-backgrounded task
/// can clear `backgroundTaskId` while retaining the output identity.
/// `timed_out_after_ms` is set only when the command hit its timeout and was
/// auto-moved to the background (claude-code 2.1.210+ `timedOutAfterMs`); it
/// carries the exceeded timeout in ms and follows the output identity fields.
fn bash_result_data(
    stdout: &str,
    stderr: &str,
    interrupted: bool,
    is_image: bool,
    return_code_interpretation: Option<&str>,
    no_output_expected: bool,
    background_task_id: Option<&str>,
    output_file: Option<&ProcessOutputFile>,
    timed_out_after_ms: Option<u64>,
    background_ends_with_final_response: bool,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert(
        "stdout".into(),
        serde_json::Value::String(stdout.to_string()),
    );
    m.insert(
        "stderr".into(),
        serde_json::Value::String(stderr.to_string()),
    );
    m.insert("interrupted".into(), serde_json::Value::Bool(interrupted));
    m.insert("isImage".into(), serde_json::Value::Bool(is_image));
    if let Some(rci) = return_code_interpretation {
        m.insert(
            "returnCodeInterpretation".into(),
            serde_json::Value::String(rci.to_string()),
        );
    }
    m.insert(
        "noOutputExpected".into(),
        serde_json::Value::Bool(no_output_expected),
    );
    if let Some(bid) = background_task_id {
        m.insert(
            "backgroundTaskId".into(),
            serde_json::Value::String(bid.to_string()),
        );
    }
    if let Some(output_file) = output_file {
        m.insert(
            "persistedOutputPath".into(),
            serde_json::Value::String(output_file.path.clone()),
        );
        m.insert(
            "persistedOutputSize".into(),
            serde_json::Value::Number(output_file.size.into()),
        );
    }
    if let Some(ms) = timed_out_after_ms {
        m.insert(
            "timedOutAfterMs".into(),
            serde_json::Value::Number(ms.into()),
        );
    }
    // claude-code 2.1.238 `backgroundEndsWithFinalResponse` — schema
    // `At(!0).optional()`, i.e. a LITERAL-`true` optional: the oracle sets it to
    // `!0` or leaves it `undefined` (`let Z=_.backgroundTaskId!==void 0 &&
    // wKo(t.agentContext)?!0:void 0`), so `false` is never serialised. It sits
    // directly after `timedOutAfterMs`/`backgroundCwdHint` in the output schema.
    if background_ends_with_final_response {
        m.insert(
            "backgroundEndsWithFinalResponse".into(),
            serde_json::Value::Bool(true),
        );
    }
    serde_json::Value::Object(m)
}

/// Port of claude-code 2.1.263 `$2t` (`src_160988549.js` @1886651) — the
/// model-facing note attached to a backgrounded command
/// (`mapToolResultToToolResultBlockParam`'s `y`). The 2.1.238 spelling was
/// `L0i`, with three head arms instead of four.
///
/// ```js
/// function $2t({backgroundTaskId:e,outputPath:t,backgroundedByUser:r,
///               backgroundedToDeliverMessage:o,timedOutAfterMs:d,
///               reapedAtFinalResponse:p,readToolName:_}){
///  let E=r?`Command was manually backgrounded by user with ID: ${e}. Output is being written to: ${t}.`
///       :o?`Command was moved to the background (ID: ${e}) so that a message that arrived while it was running can reach you; it was not interrupted. Output is being written to: ${t}.`
///       :d!==void 0?`Command did not complete within its ${Math.max(1,Math.round(d/1000))}s timeout and was moved to the background (ID: ${e}). Output is being written to: ${t}.`
///       :`Command running in background with ID: ${e}. Output is being written to: ${t}.`,
///   C=p?"If it exits while you are still working…":r?void 0:"You will be notified when it completes.",
///   I=r?void 0:`To check interim output, use ${_} on that file path.`;
///  return[E,C,I].filter(Boolean).join(" ")}
/// ```
///
/// The trigger is preserved through the foreground requester; timeout,
/// manual backgrounding and message delivery choose distinct oracle heads.
fn background_note(
    background_task_id: &str,
    output_path: &str,
    timed_out_after_ms: Option<u64>,
    reaped_at_final_response: bool,
) -> String {
    background_note_for_reason(
        background_task_id,
        output_path,
        timed_out_after_ms,
        reaped_at_final_response,
        0,
    )
}

fn background_note_for_reason(
    background_task_id: &str,
    output_path: &str,
    timed_out_after_ms: Option<u64>,
    reaped_at_final_response: bool,
    reason: u8,
) -> String {
    use platform_api::task_registry::TaskBackgroundReason;
    let head = if reason == TaskBackgroundReason::User as u8 {
        format!("Command was manually backgrounded by user with ID: {background_task_id}. Output is being written to: {output_path}.")
    } else if reason == TaskBackgroundReason::DeliverMessage as u8 {
        format!("Command was moved to the background (ID: {background_task_id}) so that a message that arrived while it was running can reach you; it was not interrupted. Output is being written to: {output_path}.")
    } else {
        match timed_out_after_ms {
        // Seconds shown = `Math.max(1, Math.round(timeoutMs / 1000))`.
        Some(ms) => {
            let secs = (((ms as f64) / 1000.0).round() as i64).max(1);
            format!(
                "Command did not complete within its {secs}s timeout and was moved to the background (ID: {background_task_id}). Output is being written to: {output_path}."
            )
        }
        None => format!(
            "Command running in background with ID: {background_task_id}. Output is being written to: {output_path}."
        ),
    }
    };
    // NOTE the U+2014 EM DASH in the reaped sentence (oracle stores it as the
    // JS escape `—`).
    let lifetime = if reaped_at_final_response {
        "If it exits while you are still working you will be notified, but it is terminated when you give your final response and no notification can follow that — so do not end your turn to wait for it; if you need its result, wait for it before giving your final response."
    } else {
        "You will be notified when it completes."
    };
    format!("{head} {lifetime} To check interim output, use Read on that file path.")
}

/// Port of claude-code 2.1.238 `wKo(agentContext)`:
/// `e!==void 0 && e.agentType==="subagent" && e.isAsync===!1` — TRUE only for a
/// SYNCHRONOUS subagent, whose backgrounded commands are reaped when it gives
/// its final response.
///
/// LingXi mapping, and its one documented residual:
/// - `agentType==="subagent"` → `ctx.agent_id.is_some()`. This is the SAME
///   discriminator the port already uses for the oracle's `v=!t.agentId`
///   (`prevent_cwd_changes` below), so the two stay consistent.
/// - `isAsync===false` → `!ctx.options.is_non_interactive_session`. The dispatch
///   invoker sets `is_non_interactive_session = is_async ||
///   effective_non_interactive_session()` (`agent/src/runner.rs`), so this is
///   exact in an interactive session and CONSERVATIVE in a headless one: a
///   synchronous subagent inside `-p`/scheduled work is classified as
///   "survives", i.e. it keeps the pre-2.1.238 wording rather than gaining a
///   false reaped warning. Making it exact needs `is_async` threaded onto
///   `ToolUseContext` (tool-api), which is outside this crate.
fn background_ends_with_final_response(ctx: &ToolUseContext) -> bool {
    ctx.agent_id.is_some() && !ctx.options.is_non_interactive_session
}

/// Build the SUCCESSFUL `tool_result` for a timed-out / interrupted Bash run,
/// mirroring claude-code's interrupted shape (`BashTool.tsx` ~602-605 / 720):
/// `interrupted: true`, `timed_out: true`, partial stdout normalized + truncated
/// through the same pipeline as the success arm, and the
/// `<error>Command was aborted before completion</error>` marker appended to
/// stderr (`BashTool.tsx:602-604`). `is_error` follows `interrupted` (TS
/// `is_error: interrupted`) and is therefore `true`.
/// claude-code `yWt` + `"wget"` (= `Les`) — the command names the backgrounding
/// telemetry reports as a `command_type`, in the oracle's order.
const TELEMETRY_COMMAND_TYPES: &[&str] = &[
    "npm",
    "yarn",
    "pnpm",
    "node",
    "python",
    "python3",
    "go",
    "cargo",
    "make",
    "docker",
    "terraform",
    "webpack",
    "vite",
    "jest",
    "pytest",
    "curl",
    "git",
    "gh",
    "dotnet",
    "msbuild",
    "nuget",
    "bun",
    "bunx",
    "npx",
    "deno",
    "pwsh",
    "pip",
    "uv",
    "poetry",
    "gradle",
    "mvn",
    "nx",
    "turbo",
    "tsc",
    "eslint",
    "prettier",
    "build",
    "test",
    "serve",
    "watch",
    "dev",
    "xcodebuild",
    "swift",
    "bazel",
    "nix",
    "nix-shell",
    "nix-build",
    "nix-env",
    "wget",
];

/// claude-code `Npe(command)` — the `command_type` field on the backgrounding
/// events:
///
/// ```js
/// function Npe(e){let t=Lm(e);if(t.length===0)return S("other");
///   for(let r of t){let o=ft(r," ");let d=Les.find((p)=>p===o);if(d)return u(d)}
///   return S("other")}
/// ```
///
/// Split the command line, take each sub-command's FIRST token, and report the
/// first one that is a known name — else `"other"`. The first MATCH wins, not
/// the first sub-command: `cd foo && cargo build` reports `cargo`.
fn telemetry_command_type(command: &str) -> &'static str {
    for part in permission::shell_command::split_command(command) {
        let head = part.trim().split(' ').next().unwrap_or_default();
        if let Some(known) = TELEMETRY_COMMAND_TYPES.iter().find(|c| **c == head) {
            return known;
        }
    }
    "other"
}

/// Build the model-facing result for a killed command. `timeout_ms = Some(ms)`
/// marks a *timed-out* command (exit `143`/`R9c`): claude's shell prepends
/// `Command timed out after <qs(ms)>` to stderr (`uqh`, space-joined) before the
/// tool layer appends the abort marker; `None` is a plain interrupt.
fn build_interrupted_result(
    stdout_partial: &str,
    stderr_partial: &str,
    cmd_str: &str,
    timeout_ms: Option<u64>,
    sandbox_violations: &[String],
) -> ToolCallResult {
    let (stdout_clean, _ansi_out) = strip_ansi_count(stdout_partial);
    let (stderr_clean, _ansi_err) = strip_ansi_count(stderr_partial);
    // Timeout annotation (claude `n(\`Command timed out after ${qs(#d)}\`)` →
    // `r.stderr = uqh(msg, prev)` = `${msg} ${prev}` when prev is non-empty).
    let stderr_clean = match timeout_ms {
        Some(ms) => {
            let annotation = format!("Command timed out after {}", format_duration_ms(ms));
            if stderr_clean.is_empty() {
                annotation
            } else {
                format!("{annotation} {stderr_clean}")
            }
        }
        None => stderr_clean,
    };
    let stderr_clean = append_sandbox_violations(&stderr_clean, sandbox_violations);
    let normalized =
        crate::shared::strip_empty_lines(&crate::shared::normalize_stdout(&stdout_clean));
    // no-truncation: A1/STEP-4. 2.1.220 hands `data.stdout` to the result
    // mapper VERBATIM (BIN off 235703218 uses `Jst()` only as the
    // `D.length>Jst()` nudge predicate); oversized output is handled by the
    // orchestrator's `<persisted-output>` layer instead. See
    // `persistence_threshold` below and `orchestrator::tool_result_persistence`.
    let stdout_final = normalized;

    // claude-code appends the abort marker to stderr, preceded by EOL when
    // stderr is non-empty (`BashTool.tsx:602-604`).
    let mut stderr_final = stderr_clean.trim_end().to_string();
    if !stderr_final.is_empty() {
        stderr_final.push('\n');
    }
    stderr_final.push_str(ABORT_MARKER);

    // Model-facing render: `[c, u(+abort marker)].join("\n")` (the binary appends
    // the abort marker inside the result mapper, so build it from the pre-marker
    // stderr with `interrupted = true`).
    let model_content = bash_model_content(&stdout_final, &stderr_clean, true, None);

    ToolCallResult {
        // A killed/interrupted command: `interrupted: true`. No exit code or
        // `returnCodeInterpretation` on kill (the binary's `p?.message` is absent).
        data: bash_result_data(
            &stdout_final,
            &stderr_final,
            true,
            false,
            None,
            crate::silent::is_silent_bash_command(cmd_str),
            None,
            None,
            None,
            false,
        ),
        model_content: Some(model_content),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

// ===== `staleReadFileStateHint` (claude-code 2.1.238 `OcT` + the `te` note) ==

/// Port of claude-code's `WRITE_COMMAND_MARKERS` regex `PcT`:
///
/// ```js
/// PcT=new RegExp(["--write","--fix","--in-place","--auto-correct",
///  "\\brun\\s+format\\b","\\brun\\s+fix\\b","\\b(yarn|pnpm)\\s+format\\b",
///  "\\blint:file\\b","\\blint:fix\\b","\\bblack\\b","\\bisort\\b",
///  "\\bruff\\s+format\\b","\\bcargo\\s+(fmt|fix)\\b","\\brustfmt\\b",
///  "\\bgo\\s+fmt\\b","\\bterraform\\s+fmt\\b","\\bdprint\\s+fmt\\b",
///  "\\bswiftformat\\b","\\bphpcbf\\b"].join("|"));
/// ```
///
/// No `i` flag ⇒ case-SENSITIVE. Hand-rolled rather than pulled through a regex
/// crate: `tool-shell` has no `regex` dependency and this alternation is only
/// bare substrings, `\b`-anchored words, and `word \s+ word` pairs.
fn command_looks_like_a_writer(command: &str) -> bool {
    const BARE: [&str; 4] = ["--write", "--fix", "--in-place", "--auto-correct"];
    const WORDS: [&str; 7] = [
        "lint:file",
        "lint:fix",
        "black",
        "isort",
        "rustfmt",
        "swiftformat",
        "phpcbf",
    ];
    // `\bA\s+B\b` pairs, in the oracle's alternation order.
    const PAIRS: [(&str, &str); 10] = [
        ("run", "format"),
        ("run", "fix"),
        ("yarn", "format"),
        ("pnpm", "format"),
        ("ruff", "format"),
        ("cargo", "fmt"),
        ("cargo", "fix"),
        ("go", "fmt"),
        ("terraform", "fmt"),
        ("dprint", "fmt"),
    ];
    BARE.iter().any(|m| command.contains(m))
        || WORDS.iter().any(|w| contains_word(command, w))
        || PAIRS.iter().any(|(a, b)| contains_word_pair(command, a, b))
}

/// ASCII `\w` — the character class JS `\b` is defined against.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True when `needle` occurs in `haystack` with a JS `\b` on BOTH sides.
fn contains_word(haystack: &str, needle: &str) -> bool {
    word_match_end(haystack, needle, 0).is_some()
}

/// Index just past the first `\b`-anchored occurrence of `needle` at or after
/// `from`, or `None`.
fn word_match_end(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || nb.len() > hb.len() {
        return None;
    }
    // Byte-wise scan: every needle here is pure ASCII, and an ASCII byte never
    // occurs inside a multi-byte UTF-8 sequence, so a byte match is always on a
    // char boundary.
    let mut start = from;
    while start + nb.len() <= hb.len() {
        if &hb[start..start + nb.len()] == nb {
            let end = start + nb.len();
            let left_ok = start == 0 || !(is_word_byte(hb[start - 1]) && is_word_byte(nb[0]));
            let right_ok =
                end == hb.len() || !(is_word_byte(hb[end]) && is_word_byte(nb[nb.len() - 1]));
            if left_ok && right_ok {
                return Some(end);
            }
        }
        start += 1;
    }
    None
}

/// True when `haystack` matches `\b<first>\s+<second>\b`.
fn contains_word_pair(haystack: &str, first: &str, second: &str) -> bool {
    let mut from = 0usize;
    while let Some(end) = word_match_end(haystack, first, from) {
        let rest = &haystack[end..];
        let ws = rest.len() - rest.trim_start().len();
        if ws > 0 {
            let after = &haystack[end + ws..];
            if after.starts_with(second) {
                let tail = end + ws + second.len();
                let hb = haystack.as_bytes();
                let sb = second.as_bytes();
                if tail == hb.len() || !(is_word_byte(hb[tail]) && is_word_byte(sb[sb.len() - 1])) {
                    return true;
                }
            }
        }
        from = end;
    }
    false
}

/// Port of Node's `path.relative(from, to)` for the two ABSOLUTE paths this
/// call site always has. Returns `""` when the paths are equal (the oracle
/// relies on that falsiness: `path.relative(cwd,X) || X`).
fn path_relative(from: &std::path::Path, to: &std::path::Path) -> String {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();
    // Different roots (e.g. another Windows drive): Node returns `to` verbatim.
    if common == 0 && !f.is_empty() && !t.is_empty() {
        return to.to_string_lossy().into_owned();
    }
    let mut parts: Vec<String> = std::iter::repeat("..".to_string())
        .take(f.len() - common)
        .collect();
    parts.extend(
        t[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// Port of claude-code 2.1.238 `OcT` + the `te` note it feeds
/// (`staleReadFileStateHint`):
///
/// ```js
/// async function OcT(e,t,r){if(!PcT.test(e))return[];let n=[];
///  return await Promise.all(Array.from(t.entries(),([o,i])=>f4e(o).then((s)=>{
///    if(s>r&&s>i.timestamp)n.push(o)}).catch(()=>{}))),n}
/// …
/// let re=await OcT(e.command,t.readFileState,i);
/// if(re.length>0){let oe=er(),fe=re.slice(0,5).map(X=>path.relative(oe,X)||X).join(", "),
///   ne=re.length>5?` and ${re.length-5} more`:"";
///   te=`[This command modified ${re.length} ${Et(re.length,"file")} you've previously read: ${fe}${ne}. Call Read before editing.]`}
/// ```
///
/// `since_ms` is the oracle's `i = Math.floor(Date.now()/1000)*1000`, sampled at
/// the top of `call`.
///
/// RESIDUAL: the registry exposes no non-promoting `(path, entry)` iterator, so
/// the LRU recency read is deferred to the CANDIDATES only — paths whose on-disk
/// mtime already bumped past `since_ms`. lru-cache's `entries()` promotes
/// nothing; here a file the command actually rewrote is promoted. Untouched
/// files (the overwhelming majority) are never `get`-ed, so eviction order is
/// unchanged in the common case.
fn stale_read_file_state_hint(
    ctx: &BuiltinToolContext,
    command: &str,
    cwd: &std::path::Path,
    since_ms: i64,
) -> Option<String> {
    if !command_looks_like_a_writer(command) {
        return None;
    }
    // `Array.from(readFileState.entries())` — lru-cache yields MRU first, which
    // is exactly `keys()`'s order here.
    let paths = {
        let state = ctx.read_file_state.lock().ok()?;
        state.keys()
    };
    let mut modified: Vec<std::path::PathBuf> = Vec::new();
    for path in paths {
        // `f4e(o)` = `Math.floor((await stat(o)).mtimeMs)`; a stat failure is
        // swallowed by the oracle's `.catch(()=>{})`.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let mtime_ms = tool_api::read_file_state::mtime_ms_floor(mtime);
        if mtime_ms <= since_ms {
            continue;
        }
        let entry_mtime = {
            let Ok(mut state) = ctx.read_file_state.lock() else {
                continue;
            };
            state.get(&path).map(|e| e.mtime_ms)
        };
        if entry_mtime.is_some_and(|t| mtime_ms > t) {
            modified.push(path);
        }
    }
    if modified.is_empty() {
        return None;
    }
    let n = modified.len();
    let listed = modified
        .iter()
        .take(5)
        .map(|p| {
            let rel = path_relative(cwd, p);
            if rel.is_empty() {
                p.to_string_lossy().into_owned()
            } else {
                rel
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    // `Et(n,"file")` — the shared pluralizer.
    let noun = if n == 1 { "file" } else { "files" };
    let more = if n > 5 {
        format!(" and {} more", n - 5)
    } else {
        String::new()
    };
    Some(format!(
        "[This command modified {n} {noun} you've previously read: {listed}{more}. Call Read before editing.]"
    ))
}

fn invalidate_written_read_state(ctx: &BuiltinToolContext, cwd: &std::path::Path, command: &str) {
    let paths = crate::command_semantics::parsed_written_paths(command);
    if paths.is_empty() {
        return;
    }
    let Ok(mut state) = ctx.read_file_state.lock() else {
        return;
    };
    for raw in paths {
        let path = std::path::Path::new(&raw);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        state.remove(&absolute);
    }
}

// ===== BASH-07 — `ghRateLimitHint` (claude-code 2.1.238 `ikf`) ==============

/// The system-reminder the oracle appends when a `gh` command reports a GitHub
/// API rate-limit error — byte-locked to `ikf`'s return value (oracle 2.1.238
/// @113553697; the same bytes in 2.1.220).
const GH_RATE_LIMIT_REMINDER: &str = "<system-reminder>GitHub API rate limit exceeded (5,000/hr shared across all tools and agents). Run `gh api rate_limit --jq .resources` and sleep until reset before further gh calls. If polling in a loop, use ScheduleWakeup instead of retrying.</system-reminder>";

/// Oracle `Y_v = 60000` — once emitted, the reminder is suppressed for a minute
/// so a retry loop does not repeat it on every call.
const GH_RATE_LIMIT_BACKOFF_MS: i64 = 60_000;

/// The subcommands the oracle's `gh`-invocation regex excludes — `V_v`'s
/// `(?!auth\b|help\b|version\b|alias\b|completion\b|config\b)`. None of them
/// spends API quota, so a rate-limit string in their output is not a hint.
const GH_RATE_LIMIT_EXCLUDED_SUBCOMMANDS: [&str; 6] =
    ["auth", "help", "version", "alias", "completion", "config"];

/// Port of the oracle's per-session `toolState.get(y7a)` (`class y7a {
/// backoffUntil = 0 }`), keyed by session id.
///
/// RESIDUAL: `ToolUseContext` exposes no generic per-session tool-state bag, so
/// the backoff lives in a process-global map keyed by
/// `BuiltinToolContext::session_id` (empty key when a context carries none —
/// tests and the session-less shims, which then share one entry exactly as they
/// share every other process-global here).
static GH_RATE_LIMIT_BACKOFF_UNTIL: Lazy<std::sync::Mutex<HashMap<String, i64>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

/// JS `\w` — `[A-Za-z0-9_]`.
fn is_js_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Port of oracle `V_v`:
/// `/(?:^|[;&|]|\b(?:then|do)\b)\s*gh\s+(?!auth\b|help\b|version\b|alias\b|completion\b|config\b)/`
/// — i.e. `gh` used as a COMMAND word (at the start, after a `;`/`&`/`|`
/// separator, or after a `then`/`do` keyword) with a quota-spending subcommand.
///
/// Hand-scanned rather than compiled: `tool-shell` pulls in no regex crate (the
/// same reason [`is_image_output`] scans by hand), and Rust's `regex` has no
/// lookahead anyway.
///
/// One JS artifact is reproduced deliberately: `\s+` is GREEDY with
/// backtracking, so when two or more whitespace characters separate `gh` from
/// its subcommand the engine can end `\s+` ON a whitespace character, where no
/// excluded keyword can match and the negative lookahead therefore always
/// succeeds. Only a SINGLE separator pins the lookahead to the subcommand.
fn command_invokes_rate_limited_gh(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i + 1 < n {
        if chars[i] != 'g' || chars[i + 1] != 'h' {
            i += 1;
            continue;
        }
        // `gh\s+` — at least one whitespace character must follow.
        let after = i + 2;
        let mut ws_end = after;
        while ws_end < n && chars[ws_end].is_whitespace() {
            ws_end += 1;
        }
        if ws_end == after {
            i += 1;
            continue;
        }
        // `(?:^|[;&|]|\b(?:then|do)\b)\s*` — walk back over the `\s*`, then
        // test the three prefix alternatives at that position.
        let mut j = i;
        while j > 0 && chars[j - 1].is_whitespace() {
            j -= 1;
        }
        let prefix_ok = if j == 0 {
            true
        } else {
            let prev = chars[j - 1];
            if prev == ';' || prev == '&' || prev == '|' {
                true
            } else if is_js_word_char(prev) && !is_js_word_char(chars[j]) {
                // The trailing `\b` of `\b(?:then|do)\b` needs a non-word
                // character at `j`; a zero-width `\s*` leaves `chars[j] == 'g'`
                // there, which correctly rejects `dogh …`.
                ["then", "do"].iter().any(|kw| {
                    let k: Vec<char> = kw.chars().collect();
                    j >= k.len()
                        && chars[j - k.len()..j] == k[..]
                        && (j == k.len() || !is_js_word_char(chars[j - k.len() - 1]))
                })
            } else {
                false
            }
        };
        if prefix_ok {
            if ws_end - after >= 2 {
                // `\s+` can backtrack onto whitespace ⇒ lookahead always passes.
                return true;
            }
            let mut k = ws_end;
            while k < n && is_js_word_char(chars[k]) {
                k += 1;
            }
            let subcommand: String = chars[ws_end..k].iter().collect();
            if !GH_RATE_LIMIT_EXCLUDED_SUBCOMMANDS.contains(&subcommand.as_str()) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `\bNEEDLE\b` over an already-lowercased haystack. Needle and JS word chars
/// are both ASCII, so byte-indexed boundary probes are safe: any byte of a
/// multi-byte UTF-8 character is `>= 0x80` and therefore a non-word byte.
fn contains_lowercase_word(haystack_lower: &str, needle_lower: &str) -> bool {
    let hb = haystack_lower.as_bytes();
    let is_word_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    haystack_lower.match_indices(needle_lower).any(|(at, _)| {
        let end = at + needle_lower.len();
        (at == 0 || !is_word_byte(hb[at - 1])) && (end >= hb.len() || !is_word_byte(hb[end]))
    })
}

/// Port of oracle `K_v`:
/// `/API rate limit (?:already )?exceeded|exceeded a secondary rate limit|\bRATE_LIMITED\b/i`.
fn output_reports_gh_rate_limit(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("api rate limit exceeded")
        || lower.contains("api rate limit already exceeded")
        || lower.contains("exceeded a secondary rate limit")
        || contains_lowercase_word(&lower, "rate_limited")
}

/// Port of claude-code 2.1.238 `ikf` — the `ghRateLimitHint` result field:
///
/// ```js
/// function ikf(e,t,r){if(!V_v.test(e)||!K_v.test(t)||Date.now()<r.backoffUntil)return;
///  return r.backoffUntil=Date.now()+Y_v,"<system-reminder>GitHub API rate limit exceeded …</system-reminder>"}
/// ```
///
/// Wired as `q=_.backgroundTaskId?void 0:ikf(e.command,S,t.toolState.get(y7a))`
/// and appended LAST to the model-facing content
/// (`[h,g,y,p,f].filter(Boolean).join("\n")`, `f` = this hint).
///
/// `S` is the oracle's FULL command output; claude-code's bash provider merges
/// the child's stderr into stdout (its analytics always report
/// `stderr_length: 0`), so the port passes both streams here.
fn gh_rate_limit_hint(
    session_key: &str,
    command: &str,
    stdout: &str,
    stderr: &str,
    now_ms: i64,
) -> Option<&'static str> {
    if !command_invokes_rate_limited_gh(command) {
        return None;
    }
    if !output_reports_gh_rate_limit(stdout) && !output_reports_gh_rate_limit(stderr) {
        return None;
    }
    let mut state = GH_RATE_LIMIT_BACKOFF_UNTIL.lock().ok()?;
    let backoff_until = state.get(session_key).copied().unwrap_or(0);
    if now_ms < backoff_until {
        return None;
    }
    state.insert(session_key.to_string(), now_ms + GH_RATE_LIMIT_BACKOFF_MS);
    Some(GH_RATE_LIMIT_REMINDER)
}

// ===== Image-output handling (claude-code `BashTool/utils.ts`) ==============

/// True when `content` is a base64 image data URI. 1:1 port of claude-code
/// `isImageOutput` (`BashTool/utils.ts:49-50`):
/// `/^data:image\/[a-z0-9.+_-]+;base64,/i`. Callers pass the trimmed,
/// model-facing stdout (matching TS, which tests `stripEmptyLines(stdout)`).
#[must_use]
fn is_image_output(content: &str) -> bool {
    // ASCII, case-insensitive — mirror the `i` flag. Manual scan avoids a regex
    // dep tool-shell doesn't pull in.
    let prefix = b"data:image/";
    let b = content.as_bytes();
    if b.len() < prefix.len() {
        return false;
    }
    if !b[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return false;
    }
    // `[a-z0-9.+_-]+` (the image subtype) — at least one char.
    let mut i = prefix.len();
    let start = i;
    while i < b.len() {
        let c = b[i];
        // The TS class is case-insensitive via the `i` flag, so accept A-Z too.
        if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'_' | b'-') {
            i += 1;
        } else {
            break;
        }
    }
    if i == start {
        return false; // empty subtype
    }
    // Followed by the literal `;base64,`.
    const SEP: &[u8] = b";base64,";
    b.len() >= i + SEP.len() && &b[i..i + SEP.len()] == SEP
}

/// Parse a `data:<media_type>;base64,<payload>` URI into `(media_type, payload)`.
/// 1:1 port of claude-code `parseDataUri` (`BashTool/utils.ts:53-65`):
/// `/^data:([^;]+);base64,(.+)$/`. Input is trimmed before matching. Returns
/// `None` when it doesn't match (so callers fall through to text handling).
/// The returned payload is already valid base64 of the image bytes, so it can
/// be handed straight to [`protocol::ImageSource::Base64`] with no re-encode.
#[must_use]
fn parse_data_uri(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let rest = s.strip_prefix("data:")?;
    // media_type = `[^;]+` up to the first `;`.
    let semi = rest.find(';')?;
    if semi == 0 {
        return None; // empty media type
    }
    let media_type = &rest[..semi];
    // Must be exactly `;base64,` after the media type, then a non-empty payload
    // (`(.+)$`).
    let payload = rest[semi..].strip_prefix(";base64,")?;
    if payload.is_empty() {
        return None;
    }
    Some((media_type.to_string(), payload.to_string()))
}

/// Validate a shell image data URI and resize it to the provider's byte and
/// dimension budget. The returned tuple is `(normalized_uri, base64_payload)`.
/// A magic-byte-valid but decoder-rejected image preserves the existing
/// fail-later behavior instead of silently converting the command output to
/// text.
fn prepare_shell_image_output(content: &str) -> Option<(String, String)> {
    use base64::Engine as _;

    let (claimed_media_type, payload) = parse_data_uri(content)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload)
        .ok()?;
    sniff_image_media_type(&bytes)?;
    match tool_api::util::image_budget::process_image(bytes) {
        Ok(processed) => {
            let uri = format!("data:{};base64,{}", processed.media_type, processed.base64);
            Some((uri, processed.base64))
        }
        Err(_) => Some((
            format!("data:{claimed_media_type};base64,{payload}"),
            payload,
        )),
    }
}

/// Image magic-byte sniff (claude `Wfe`) — shared impl in
/// [`tool_api::util::image_sniff`]; the Bash gate and the dispatch loop's
/// `hKn` mapper must agree, so both use the same fn.
pub use tool_api::util::image_sniff::sniff_image_media_type;

// ===== BASH.1 — extended-glob disable prefix (SECURITY) =====================

/// Return the shell command that disables extended-glob expansion for the
/// given shell, or `None` for an unknown shell. 1:1 port of claude-code
/// `bashProvider.ts` `getDisableExtglobCommand`.
///
/// Extended globs (bash `extglob`, zsh `EXTENDED_GLOB`) can be exploited via
/// malicious filenames that expand *after* our security validation, so this
/// prefix is prepended to the user command before it is spawned.
#[must_use]
pub fn disable_extglob_command(shell_path: &str) -> Option<String> {
    // When LINGXI_SHELL_PREFIX is set, the wrapper may run a different
    // shell than `shell_path`, so emit commands for BOTH shells. Redirect
    // stdout+stderr because zsh's `command_not_found_handler` writes to stdout.
    if std::env::var("LINGXI_SHELL_PREFIX").is_ok_and(|v| !v.is_empty()) {
        return Some(
            "{ shopt -u extglob || setopt NO_EXTENDED_GLOB; } >/dev/null 2>&1 || true".into(),
        );
    }
    if shell_path.contains("bash") {
        Some("shopt -u extglob 2>/dev/null || true".into())
    } else if shell_path.contains("zsh") {
        Some("setopt NO_EXTENDED_GLOB 2>/dev/null || true".into())
    } else {
        // Unknown shell — we don't know the right command.
        None
    }
}

// ===== BASH.2 — Windows null-redirect rewrite ===============================

#[inline]
fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Rewrite Windows CMD-style `>nul` redirects to POSIX `/dev/null`. 1:1 port of
/// claude-code `shellQuoting.ts` `rewriteWindowsNullRedirect`, implementing the
/// regex `/(\d?&?>+\s*)[Nn][Uu][Ll](?=\s|$|[|&;)\n])/g` → `$1/dev/null`
/// (no `regex` crate dep; ASCII whitespace approximates JS `\s`).
///
/// Matches `>nul`, `> NUL`, `2>nul`, `&>nul`, `>>nul` (case-insensitive); does
/// NOT match `>null`, `>nullable`, `>nul.txt`, or `cat nul.txt`.
#[must_use]
pub fn rewrite_windows_null_redirect(command: &str) -> String {
    let b = command.as_bytes();
    let n = b.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut p = 0;
    while p < n {
        if let Some(group1_end) = match_null_redirect(b, p) {
            // group1 = b[p..group1_end] (the `\d?&?>+\s*` prefix), then `nul`.
            out.extend_from_slice(&b[p..group1_end]);
            out.extend_from_slice(b"/dev/null");
            p = group1_end + 3; // skip the matched `nul`
        } else {
            out.push(b[p]);
            p += 1;
        }
    }
    // Replacements only ever touch ASCII spans, so the bytes remain valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| command.to_owned())
}

/// Try to match `(\d?&?>+\s*)nul(?=\s|$|[|&;)\n])` at byte offset `p`. On
/// success returns the byte offset where `nul` begins (i.e. the end of
/// group 1); the caller knows the literal `nul` is exactly 3 bytes.
fn match_null_redirect(b: &[u8], p: usize) -> Option<usize> {
    let n = b.len();
    let mut q = p;
    // \d? — optional single digit
    if q < n && b[q].is_ascii_digit() {
        q += 1;
    }
    // &? — optional single ampersand
    if q < n && b[q] == b'&' {
        q += 1;
    }
    // >+ — one or more redirects (required)
    let gt_start = q;
    while q < n && b[q] == b'>' {
        q += 1;
    }
    if q == gt_start {
        return None;
    }
    // \s* — optional whitespace
    while q < n && is_ascii_ws(b[q]) {
        q += 1;
    }
    let nul_start = q;
    // nul (case-insensitive), exactly 3 chars
    if nul_start + 3 > n {
        return None;
    }
    if !(b[nul_start].eq_ignore_ascii_case(&b'n')
        && b[nul_start + 1].eq_ignore_ascii_case(&b'u')
        && b[nul_start + 2].eq_ignore_ascii_case(&b'l'))
    {
        return None;
    }
    let after = nul_start + 3;
    // Lookahead: \s | end-of-string | [|&;)\n]
    let boundary = after == n
        || is_ascii_ws(b[after])
        || matches!(b[after], b'|' | b'&' | b';' | b')' | b'\n');
    if !boundary {
        return None;
    }
    Some(nul_start)
}

/// Whether the FIRST word of the FIRST statement is `sleep`.
///
/// Port of claude-code `$es` (2.1.263 `src_160988549.js` @4326943) over
/// `Des = ["sleep"]`: an empty command, or one whose first segment has no first
/// word, is auto-backgroundable (both of `$es`'s early `return !0`s). This is
/// deliberately NOT the sleep-block predicate, which additionally requires a
/// standalone `sleep N` with N above a threshold.
fn first_statement_is_sleep(command: &str) -> bool {
    permission::shell_command::split_command(command)
        .first()
        .and_then(|segment| segment.split_whitespace().next().map(str::to_string))
        .is_some_and(|word| word == "sleep")
}

/// Settles a background shell's task record when its child is reaped.
///
/// claude-code `cnr` — how long a foreground command must run before it earns a
/// `local_bash` row (2.1.263 `src_160988549.js` @4321443: `cnr=2000`).
const FOREGROUND_ARMING_DELAY: std::time::Duration = std::time::Duration::from_millis(2000);

/// The registry's end of claude-code `I_t`'s `t.background(e)`.
///
/// Firing the notify is all it takes: the process runner is already waiting on
/// it alongside the deadline, and takes the identical move-to-background path.
struct BackgroundBashRequester {
    notify: std::sync::Arc<tokio::sync::Notify>,
    reason: std::sync::Arc<std::sync::atomic::AtomicU8>,
    settled: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
}

struct BackgroundRequestSettled(std::sync::Arc<tokio::sync::watch::Sender<bool>>);
impl Drop for BackgroundRequestSettled {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

#[async_trait::async_trait]
impl platform_api::task_registry::TaskBackgrounder for BackgroundBashRequester {
    async fn background(&self) {
        self.background_with_reason(platform_api::task_registry::TaskBackgroundReason::User)
            .await;
    }
    async fn background_with_reason(
        &self,
        reason: platform_api::task_registry::TaskBackgroundReason,
    ) {
        // First trigger wins, so an arriving message cannot relabel Ctrl+B.
        let _ = self.reason.compare_exchange(
            0,
            reason as u8,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        let mut settled = self.settled.subscribe();
        self.notify.notify_one();
        // Do not cancel the owning turn until its process has actually detached.
        while !*settled.borrow_and_update() {
            if settled.changed().await.is_err() {
                break;
            }
        }
    }
}

/// The live arming of one foreground command: the 2 s timer, plus what it takes
/// to withdraw the row again.
struct ForegroundArming {
    timer: Option<tokio::task::JoinHandle<()>>,
    withdraw_on_drop: bool,
    task_id: String,
    registry: std::sync::Arc<dyn platform_api::task_registry::TaskRegistryHandle>,
}

impl ForegroundArming {
    /// claude-code `W6t` — the command finished in the foreground, so cancel the
    /// timer and withdraw the row if it ever appeared.
    ///
    /// Withdrawing is not optional: a row left behind is `running` forever, and
    /// the registry would eventually narrate a completion for a command the
    /// model was never told had started.
    async fn join_timer(&mut self) {
        if let Some(timer) = self.timer.as_mut() {
            timer.abort();
            // Keep the handle in self until joined: dropping this await must
            // still let Drop serialize withdrawal after the timer terminates.
            let _ = timer.await;
        }
        self.timer = None;
    }

    async fn disarm(mut self) {
        self.join_timer().await;
        self.registry
            .unregister_foreground_bash(&self.task_id)
            .await;
        self.withdraw_on_drop = false;
    }

    async fn keep_for_background(mut self) {
        self.join_timer().await;
        self.withdraw_on_drop = false;
    }
}

impl Drop for ForegroundArming {
    fn drop(&mut self) {
        let timer = self.timer.take();
        if let Some(timer) = &timer {
            timer.abort();
        }
        if !self.withdraw_on_drop {
            return;
        }
        let registry = self.registry.clone();
        let task_id = self.task_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(timer) = timer {
                    let _ = timer.await;
                }
                // This only withdraws an unpromoted foreground row. It neither
                // kills a process nor removes an acknowledged background task.
                registry.unregister_foreground_bash(&task_id).await;
            });
        }
    }
}

/// Port of claude-code `Ger` (2.1.263 `src_160988549.js` @4284565): the shell's
/// result promise drives the record to its terminal status, which is what makes
/// the completion `<task-notification>` fire.
struct BackgroundBashExitSink {
    registry: std::sync::Arc<dyn platform_api::task_registry::TaskRegistryHandle>,
}

#[async_trait::async_trait]
impl platform_api::BackgroundExitSink for BackgroundBashExitSink {
    async fn on_supervised_start(&self, task_id: &str) {
        self.registry.mark_shell_supervised(task_id).await;
    }

    async fn on_supervised_exit(&self, task_id: &str, exit_code: Option<i32>) {
        self.registry.mark_shell_supervised(task_id).await;
        self.on_exit(task_id, exit_code).await;
    }

    fn manages_output(&self) -> bool {
        true
    }

    async fn append_output(
        &self,
        task_id: &str,
        content: &str,
    ) -> Result<(), platform_api::ProcessError> {
        self.registry
            .append_bash_output(task_id, content)
            .await
            .map_err(|error| platform_api::ProcessError::Io(error.to_string()))
    }

    async fn finalize_persisted_output(
        &self,
        task_id: &str,
        max_bytes: u64,
    ) -> Result<Option<u64>, platform_api::ProcessError> {
        self.registry
            .finalize_persisted_output(task_id, max_bytes)
            .await
            .map_err(|error| platform_api::ProcessError::Io(error.to_string()))
    }

    async fn flush_output(&self, task_id: &str) -> Result<(), platform_api::ProcessError> {
        self.registry
            .flush_bash_output(task_id)
            .await
            .map_err(|error| platform_api::ProcessError::Io(error.to_string()))
    }

    async fn on_stall(&self, task_id: &str, tail: &str) {
        self.registry.notify_bash_stall(task_id, tail).await;
    }

    async fn on_memory_pressure(&self, task_id: &str) -> bool {
        self.registry.claim_bash_memory_pressure_stop(task_id).await
    }

    async fn on_exit(&self, task_id: &str, exit_code: Option<i32>) {
        // Best-effort: a settle for a since-evicted task is a benign NotFound.
        let _ = self
            .registry
            .settle_background_bash(task_id, exit_code, false)
            .await;
    }
}

/// Terminates a backgrounded shell's OS process on `TaskStop` or teardown.
struct BackgroundBashKiller {
    process: std::sync::Arc<dyn platform_api::ProcessRunner>,
    handle: platform_api::ProcessHandle,
}

#[async_trait::async_trait]
impl platform_api::task_registry::TaskKiller for BackgroundBashKiller {
    async fn kill(&self) {
        let _ = self.process.kill(&self.handle).await;
    }
}

/// Compute the per-task output file path used when `run_in_background=true`.
///
/// Mirrors `platform_posix::process::task_output_path` (which we
/// cannot depend on from this crate without forming a Cargo cycle —
/// lingxi-tools → lingxi-platform-posix → lingxi-lsp → lingxi-tools).
#[must_use]
pub fn task_output_path(task_id: &str) -> PathBuf {
    platform_api::task_output::legacy_output_path(task_id)
}

/// A per-call id, unique within the process AND across processes.
///
/// The clock alone is NOT enough. This used to be `nanos ^ pid`, and
/// `SystemTime::now()` does not advance on every call — two Bash tool calls
/// issued concurrently (which the model does routinely; parallel tool calls
/// are a supported feature) could land in the same tick and produce the SAME
/// id. Both then used the same `/tmp/claude-<id>-cwd` readback file, and the
/// first call to finish DELETED it during cleanup before the second read it —
/// so a `cd` silently failed to persist to the next call.
///
/// It surfaced as a ~1-in-8 flake in `cwd_persistence.rs` under CPU load and
/// was easy to mistake for test flakiness; it is a real concurrency defect in
/// the tool. The atomic counter makes collisions impossible within a process,
/// and the pid keeps ids distinct between them.
fn ephemeral_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mix = (nanos as u64) ^ u64::from(std::process::id());
    format!("{prefix}-{mix:016x}-{seq:x}")
}

fn cmd_hash(s: &str) -> String {
    // Lightweight FNV-1a; cryptographic strength not required — used solely
    // for `_PROTO_command_hash` routing in telemetry.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

async fn emit_failed(
    bus: &telemetry::AnalyticsBus,
    request_id: &str,
    error_kind: &str,
    started_at: SystemTime,
) {
    let elapsed_ms = SystemTime::now()
        .duration_since(started_at)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut meta: LogEventMetadata = HashMap::new();
    meta.insert(
        "request_id".into(),
        AnalyticsValue::String(request_id.into()),
    );
    meta.insert(
        "error_kind".into(),
        AnalyticsValue::String(error_kind.into()),
    );
    meta.insert("duration_ms".into(), AnalyticsValue::Int(elapsed_ms as i64));
    bus.log_event(BASH_FAILED, meta).await;
}

// ===== BASH.7 — model-gated prompt selector (`Dh` predicate) ===============
//
// claude-code builds the Bash tool's model-facing prompt as
// `getSimplePrompt(model) = Dh(model) ? qUp(/*CONCISE*/) : TXa(/*VERBOSE*/)`.
// `Dh` (binary offset ~195159752) is the "simple system prompt" gate that
// selects the SHORT (current-gen) vs LONG (classic) variant. It is the shared
// `tool_api::dh_simple_system_prompt` (single source of truth in
// `tool-api/src/model_prompt_gate.rs`), consulted identically by the WebSearch
// and file/task tools — the `UWu`/`dfe`/`FWu` parity notes live there. The
// Bash `prompt()` method calls it directly at its model gate.

// ===== Tool type ============================================================

/// `BashTool` — spawn a shell command through the configured `ProcessRunner`,
/// optionally wrapping it via the M2-04 sandbox decision matrix.
#[derive(Clone)]
pub struct BashTool {
    ctx: BuiltinToolContext,
    /// Persistent shell working directory (claude-code `STATE.cwd`).
    ///
    /// A foreground `cd` updates this via a `pwd -P` readback after the
    /// command runs, so subsequent Bash calls inherit the new directory
    /// (the tool registry holds one long-lived `Arc<BashTool>` per session,
    /// so this `Arc<Mutex<..>>` field persists across calls — BASH.4 Design B).
    ///
    /// Design-B local-to-`BashTool` divergence: claude-code keeps this
    /// session-global (`STATE.cwd`) and shares it with the permission gate; we
    /// keep it `BashTool`-local until a permission-gate live-cwd consumer is
    /// wired. Observable behavior is identical today since nothing else
    /// consumes a shared session-cwd yet.
    shell_cwd: std::sync::Arc<std::sync::Mutex<std::path::PathBuf>>,
    /// The session cwd (`ctx.cwd()`) this `BashTool` last saw, used to detect
    /// an `EnterWorktree`/`ExitWorktree` swap (worktree parity plan, Task 4).
    ///
    /// `shell_cwd` above and `ctx.cwd()` (the live `SessionCwd` cell) can
    /// diverge for two completely different reasons that need OPPOSITE
    /// handling: (a) the model ran `cd` inside the current session cwd — the
    /// persistent shell legitimately moved and must NOT be clobbered back; or
    /// (b) a worktree tool called `session_cwd.swap(..)` — claude-code's
    /// `process.chdir(worktree)` moves the *entire process*, so the persistent
    /// shell must follow. Comparing `shell_cwd` to `ctx.cwd()` directly can't
    /// tell these apart (case (a) also makes them differ). So we track the
    /// session cwd we last *observed* here; only when THAT has moved since the
    /// previous call do we know a swap (not a `cd`) happened, and re-point
    /// `shell_cwd` to match — see the call-site comment in `call()`.
    synced_session_cwd: std::sync::Arc<std::sync::Mutex<std::path::PathBuf>>,
    /// Optional `CwdChanged` hook firer (default `None` => strict no-op).
    ///
    /// Closes the seam claude-code's `onCwdChangedForHooks(cwd, newCwd)`
    /// (`Shell.ts:409`) fills: when the `pwd -P` readback below moves the cwd,
    /// fire the `CwdChanged` hook best-effort. The firer can NOT ride the
    /// construction `BuiltinToolContext` (that struct is built by a full literal
    /// in the FORBIDDEN `engine-mobile` code — a new field would break it), so
    /// it is injected via [`with_cwd_changed_firer`](BashTool::with_cwd_changed_firer),
    /// leaving the `BashTool::new` signature unchanged. The desktop composition
    /// root wires it over the shared `Arc<HookExecutorImpl>`; every other caller
    /// (mobile, tests) leaves it `None` and the fire is a no-op.
    cwd_changed_firer: hooks::OptionalCwdChangedFirer,
}

impl BashTool {
    /// Mint the task identity for this shell command: a registry task id and an
    /// output file for the process runner to append to.
    ///
    /// Returns `None` when no registry is wired, in which case the runner keeps
    /// minting its own identity and behaviour is unchanged.
    async fn allocate_task_identity(&self) -> Option<(String, String)> {
        let registry = self.ctx.task_registry.as_ref()?;
        match registry.allocate_bash_output().await {
            Ok(handle) => Some((handle.task_id, handle.output_path)),
            Err(error) => {
                // Fail open: a registry that cannot mint an identity must not
                // stop the command from running, it only costs this command its
                // registry record.
                tracing::warn!(
                    target: "tool_shell::bash",
                    %error,
                    "could not allocate a task identity for the shell command; falling back to a runner-owned id"
                );
                None
            }
        }
    }

    /// Bind the [`BackgroundTaskBinding`] for an allocated identity onto the
    /// command, so the runner writes to that file and reports that id.
    ///
    /// [`BackgroundTaskBinding`]: platform_api::BackgroundTaskBinding
    fn bind_identity(
        &self,
        sandboxed: platform_api::SandboxedCommand,
        bound: Option<&(String, String)>,
        on_demand: Option<std::sync::Arc<tokio::sync::Notify>>,
    ) -> platform_api::SandboxedCommand {
        let Some((task_id, output_path)) = bound else {
            return sandboxed;
        };
        sandboxed.with_background_task(platform_api::BackgroundTaskBinding {
            task_id: task_id.clone(),
            output_path: std::path::PathBuf::from(output_path),
            on_exit: self.ctx.task_registry.clone().map(|registry| {
                std::sync::Arc::new(BackgroundBashExitSink { registry })
                    as std::sync::Arc<dyn platform_api::BackgroundExitSink>
            }),
            on_demand,
        })
    }

    /// Create the `local_bash` record for an identity that is actually being
    /// backgrounded (claude-code `Xne`). Foreground commands never get a record.
    async fn register_background_task(
        &self,
        ctx: &ToolUseContext,
        bound: Option<&(String, String)>,
        command: &str,
        description: Option<&str>,
        cwd: &std::path::Path,
    ) -> Result<(), ToolError> {
        let (Some((task_id, _)), Some(registry)) = (bound, self.ctx.task_registry.as_ref()) else {
            return Ok(());
        };
        let registration = platform_api::task_registry::BackgroundBashRegistration {
            command: command.to_string(),
            // claude-code stores `description || command` on the record
            // (`Xne`'s `description: Me || ve`); the completion summary renders
            // it as `Background command "{description}" completed (exit code N)`.
            description: description
                .filter(|d| !d.is_empty())
                .unwrap_or(command)
                .to_string(),
            tool_use_id: ctx.tool_use_id.as_ref().map(ToString::to_string),
            // The directory the child is actually spawned in — NOT a fresh read
            // of `shell_cwd`, which ignores a subagent's per-call `ctx.cwd` and
            // would advertise a directory the command never ran in.
            cwd: Some(cwd.display().to_string()),
            // The launching agent owns the task; `None` means the main session
            // does (claude-code stamps the record's `agentId` the same way).
            creator_agent_id: ctx.agent_id,
        };
        registry
            .register_background_bash(task_id, registration)
            .await
            .map_err(|error| ToolError::Io(format!("could not register background shell: {error}")))
    }

    /// Arm the 2 s foreground record (claude-code `U6t`, fired once the poll
    /// loop sees the command has been running for `cnr` = 2000 ms).
    ///
    /// Nothing is registered at spawn: arming immediately would flash a
    /// `/tasks` row for every `ls` and `git status`. Only a command that is
    /// still running after two seconds becomes addressable, and it is withdrawn
    /// again by [`ForegroundArming::disarm`] if it then finishes in the
    /// foreground.
    fn arm_foreground_record(
        &self,
        ctx: &ToolUseContext,
        task_id: &str,
        command: &str,
        description: Option<&str>,
        notify: std::sync::Arc<tokio::sync::Notify>,
        reason: std::sync::Arc<std::sync::atomic::AtomicU8>,
        settled: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
        progress: ToolProgressSender,
    ) -> Option<ForegroundArming> {
        let registry = self.ctx.task_registry.clone()?;
        let registration = platform_api::task_registry::BackgroundBashRegistration {
            command: command.to_string(),
            description: description
                .filter(|d| !d.is_empty())
                .unwrap_or(command)
                .to_string(),
            tool_use_id: ctx.tool_use_id.as_ref().map(ToString::to_string),
            cwd: Some(self.shell_cwd.lock().unwrap().display().to_string()),
            creator_agent_id: ctx.agent_id,
        };
        let auto_background_armed =
            !crate::prompt::background_tasks_disabled() && !first_statement_is_sleep(command);
        let id = task_id.to_string();
        let arming_registry = registry.clone();
        let tool_use_id = ctx.tool_use_id.clone();
        let hint_enabled = !crate::prompt::background_tasks_disabled();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(FOREGROUND_ARMING_DELAY).await;
            if let Err(error) = arming_registry
                .register_foreground_bash(&id, registration, auto_background_armed)
                .await
            {
                tracing::debug!(
                    target: "tool_shell::bash",
                    %error,
                    "could not arm the foreground shell record"
                );
                return;
            }
            // Only now is the row addressable, so bind the handle that moves it
            // to the background before anything can ask for that.
            let _ = arming_registry
                .bind_background_requester(
                    &id,
                    std::sync::Arc::new(BackgroundBashRequester {
                        notify,
                        reason,
                        settled,
                    }),
                )
                .await;
            if hint_enabled {
                if let Some(tool_use_id) = tool_use_id {
                    let _ = progress.try_send(tool_api::progress::ToolProgress {
                        data: serde_json::json!({"kind":"background_hint", "toolUseId":tool_use_id.to_string()}),
                        tool_use_id,
                    });
                }
            }
        });
        Some(ForegroundArming {
            timer: Some(timer),
            withdraw_on_drop: true,
            task_id: task_id.to_string(),
            registry,
        })
    }

    /// Keep the minted identity only if the process runner actually used it.
    ///
    /// A runner that ignores the binding writes the child's output to a file of
    /// its own and reports its own id; claiming the registry path in that case
    /// would advertise a file nothing ever writes to. Drop the allocation
    /// instead and fall back to the runner's identity.
    async fn take_honoured_identity(
        &self,
        bound: Option<(String, String)>,
        handle: &platform_api::ProcessHandle,
    ) -> Option<(String, String)> {
        let (task_id, output_path) = bound?;
        if handle.task_id == task_id {
            return Some((task_id, output_path));
        }
        if let Some(registry) = self.ctx.task_registry.as_ref() {
            registry.discard_bash_output(&task_id).await;
        }
        tracing::debug!(
            target: "tool_shell::bash",
            minted = %task_id,
            reported = %handle.task_id,
            "process runner did not honour the bound task identity; keeping its own"
        );
        None
    }

    /// Attach the killer for a backgrounded shell so `TaskStop` and teardown can
    /// terminate a child the registry did not spawn itself.
    ///
    /// `task_id` is the id the REGISTRY knows; `handle` is what the runner needs
    /// to signal the process, and the two differ when the runner ignored the
    /// binding.
    async fn bind_background_task(
        &self,
        task_id: &str,
        handle: &platform_api::ProcessHandle,
    ) -> Result<(), ToolError> {
        let Some(registry) = self.ctx.task_registry.as_ref() else {
            return Ok(());
        };
        let killer = std::sync::Arc::new(BackgroundBashKiller {
            process: self.ctx.process.clone(),
            handle: handle.clone(),
        }) as std::sync::Arc<dyn platform_api::task_registry::TaskKiller>;
        registry
            .bind_background_process(task_id, handle.pid, killer)
            .await
            .map_err(|error| ToolError::Io(format!("could not bind background shell: {error}")))
    }

    /// Registration is the receipt that permits a supervisor to outlive this host.
    #[allow(clippy::too_many_arguments)]
    async fn acknowledge_background_task(
        &self,
        ctx: &ToolUseContext,
        bound: Option<&(String, String)>,
        command: &str,
        description: Option<&str>,
        cwd: &std::path::Path,
        handle: &platform_api::ProcessHandle,
    ) -> Result<String, ToolError> {
        let task_id = bound.map_or_else(|| handle.task_id.clone(), |(id, _)| id.clone());
        let result = async {
            self.register_background_task(ctx, bound, command, description, cwd)
                .await?;
            if bound.is_some() {
                self.bind_background_task(&task_id, handle).await?;
            }
            self.ctx
                .process
                .acknowledge_shell(handle)
                .await
                .map_err(|error| {
                    ToolError::Io(format!("could not acknowledge background shell: {error}"))
                })
        }
        .await;
        if let Err(error) = result {
            let _ = self.ctx.process.kill(handle).await;
            if let (Some(registry), Some((id, _))) = (self.ctx.task_registry.as_ref(), bound) {
                let _ = registry.settle_background_bash(id, None, true).await;
                registry.discard_bash_output(id).await;
            }
            return Err(error);
        }
        Ok(task_id)
    }

    /// Construct a fresh tool bound to the given builtin context.
    ///
    /// The `CwdChanged` firer defaults to `None` (no fire). Inject one via
    /// [`with_cwd_changed_firer`](BashTool::with_cwd_changed_firer) — the
    /// signature here is deliberately UNCHANGED so the `engine-mobile` literal
    /// and every other `BashTool::new(ctx)` caller keep compiling untouched.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        tool_api::builtin_context::install_shell_discovery_logging();
        let boot_cwd = ctx.cwd();
        let shell_cwd = std::sync::Arc::new(std::sync::Mutex::new(boot_cwd.clone()));
        let synced_session_cwd = std::sync::Arc::new(std::sync::Mutex::new(boot_cwd));
        Self {
            ctx,
            shell_cwd,
            synced_session_cwd,
            cwd_changed_firer: None,
        }
    }

    /// Inject the `CwdChanged` hook firer (builder; default is `None`).
    ///
    /// The desktop composition root calls this over the SAME
    /// `Arc<HookExecutorImpl>` the orchestrator fires its other hooks through,
    /// so a `cd` inside a Bash call fires the `CwdChanged` hook
    /// (`onCwdChangedForHooks`, `Shell.ts:409`). Optional + additive: callers
    /// that skip it (mobile, tests) keep the strict no-op.
    #[must_use]
    pub fn with_cwd_changed_firer(
        mut self,
        firer: std::sync::Arc<dyn hooks::CwdChangedFirer>,
    ) -> Self {
        self.cwd_changed_firer = Some(firer);
        self
    }

    /// Adopt the shared live-cwd cell as this tool's persistent shell cwd
    /// (builder; default keeps the private per-`BashTool` cell).
    ///
    /// This makes `BashTool` the single writer of claude-code's session-global
    /// `getCwd()`/`setCwdState` (`Pt.cwd`): a foreground `cd` commits the
    /// post-`cd` directory into the SAME cell the file/search/LSP tools read as
    /// their live cwd and the orchestrator reads for hook payloads / JSONL — so
    /// there is one live cwd, not a `BashTool`-local one plus a firer-tracked
    /// copy. The desktop composition root passes the same `current_cwd` cell it
    /// hands the `CwdChanged` firer and the orchestrator; mobile / tests skip
    /// this and keep the private cell (byte-identical isolated behavior).
    #[must_use]
    pub fn with_live_cwd(mut self, cell: tool_api::LiveCwdCell) -> Self {
        self.shell_cwd = cell;
        self
    }

    /// The oracle's `BY(input)` for THIS tool's live context — "will this call
    /// actually be sandbox-wrapped?".
    ///
    /// Shares the exact inputs [`Tool::call`] feeds
    /// [`sandbox::decision::should_use_sandbox`], so the permission decision and
    /// the execution decision cannot drift: `ctx.sandbox_available`, the
    /// `/sandbox`-aware [`BuiltinToolContext::effective_sandbox_runtime`], and
    /// the session cwd.
    ///
    /// `override_flag` is the `dangerouslyDisableSandbox` value to evaluate,
    /// letting BASH-10 run the oracle's two probes
    /// (`BY(e)` and `BY({...e, dangerouslyDisableSandbox:!1})`) over one config
    /// snapshot each.
    fn sandbox_wraps(&self, input: &Value, override_flag: bool) -> bool {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            // `if(!e.command) return !1`.
            return false;
        };
        let runtime = self.ctx.effective_sandbox_runtime();
        matches!(
            sandbox::decision::should_use_sandbox(
                command,
                self.ctx.sandbox_available,
                override_flag,
                runtime.are_unsandboxed_commands_allowed(),
                &runtime,
                self.ctx.cwd(),
            ),
            sandbox::decision::SandboxDecision::Sandbox { .. }
        )
    }

    /// `BY(e)` — honours the input's own `dangerouslyDisableSandbox`.
    fn will_sandbox(&self, input: &Value) -> bool {
        self.sandbox_wraps(
            input,
            input
                .get("dangerouslyDisableSandbox")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    }

    /// `BY({...e, dangerouslyDisableSandbox:!1})` — the counterfactual probe.
    fn will_sandbox_ignoring_override(&self, input: &Value) -> bool {
        self.sandbox_wraps(input, false)
    }
}

/// Byte-locked `message` of the oracle's sandbox-override ask
/// (2.1.238 BIN off **114873408**, 4 hits; identical in 2.1.220).
pub const SANDBOX_OVERRIDE_ASK_MESSAGE: &str = "Run outside of the sandbox";

/// `V.CLAUDE_CODE_BASH_SANDBOX_SHOW_INDICATOR` (LingXi:
/// `LINGXI_BASH_SANDBOX_SHOW_INDICATOR`) evaluated with the oracle's PLAIN JS
/// truthiness — the `userFacingName` arm reads it as `V.X && BY(e)`, not through
/// `isEnvTruthy`, so every non-empty value (including `"0"` and `"false"`)
/// enables the `SandboxedBash` indicator. Unset in a stock install ⇒ inert.
fn bash_sandbox_show_indicator() -> bool {
    std::env::var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR").is_ok_and(|v| !v.is_empty())
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "command":           { "type": "string", "description": "The command to execute" },
            // claude-code `BashTool.tsx:229` names this param `timeout`
            // (milliseconds). A model sending `timeout` must be honored.
            // claude-code: `VF(E.number().optional()).describe(...)` — NO `.max()`
            // (the "(max …)" is advisory describe text only). The max is the
            // dynamic `Wdt()`=`j3n()` value, env-overridable via BASH_MAX_TIMEOUT_MS.
            "timeout":           {
                "type": "number",
                "description": format!("Optional timeout in milliseconds (max {})", bash_max_timeout_ms())
            },
            // Property ORDER is byte-significant: `serde_json` is built with
            // `preserve_order`, so this insertion order is what serialises into
            // the tool definition. claude-code 2.1.238 `Qhm` orders
            // `command, timeout, description, run_in_background,
            // dangerouslyDisableSandbox`.
            "description":       { "type": "string", "description": "Clear, concise description of what this command does in active voice. Never use words like \"complex\" or \"risk\" in the description - just describe what it does.\n\nFor simple commands (git, npm, standard CLI tools), keep it brief (5-10 words):\n- ls → \"List files in current directory\"\n- git status → \"Show working tree status\"\n- npm install → \"Install package dependencies\"\n\nFor commands that are harder to parse at a glance (piped commands, obscure flags, etc.), add enough context to clarify what it does:\n- find . -name \"*.tmp\" -exec rm {} \\; → \"Find and delete all .tmp files recursively\"\n- git reset --hard origin/main → \"Discard all local changes and match remote main\"\n- curl -s url | jq '.data[]' → \"Fetch JSON from URL and extract data array elements\"" },
            "run_in_background": { "type": "boolean", "description": "Set to true to run this command in the background." },
            // BASH.5: 1:1 with claude-code `BashTool.tsx` schema —
            // `dangerouslyDisableSandbox: z.boolean().optional().describe(...)`.
            "dangerouslyDisableSandbox": {
                "type": "boolean",
                "description": "Set this to true to dangerously override sandbox mode and run commands without sandboxing."
            }
        },
        "required": ["command"],
        // R-MINOR: claude-code's Bash input schema is `E.strictObject(...)`
        // (additionalProperties:false) — unknown keys are rejected.
        "additionalProperties": false
    })
});

/// The Bash input schema with `run_in_background` OMITTED — claude-code 2.1.263
/// `gnr` (`src_160988549.js` @4323900):
///
/// ```js
/// gnr=m(()=>(Dl()?dnr().omit({run_in_background:!0,_simulatedSedEdit:!0})
///                 :dnr().omit({_simulatedSedEdit:!0})).superRefine(…))
/// ```
///
/// When background tasks are disabled the oracle strips the property from the
/// tool definition entirely, so the model is never offered a parameter the
/// prompt no longer explains (`getBackgroundUsageNote` already drops its bullet
/// on the same switch). Derived from [`INPUT_SCHEMA`] so the two can never drift
/// in contents or KEY ORDER (`serde_json` is built with `preserve_order`, and
/// removing one key leaves the rest in their original insertion order).
static INPUT_SCHEMA_NO_BACKGROUND: Lazy<Value> = Lazy::new(|| {
    let mut schema = INPUT_SCHEMA.clone();
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.remove("run_in_background");
    }
    schema
});

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("execute shell commands")
    }

    /// claude-code 2.1.263 `gnr` selects between the full schema and one with
    /// `run_in_background` omitted, keyed on `Dl()` (background tasks disabled).
    /// Evaluated per call — like the oracle's `we(...)` memo, which re-reads the
    /// same switch — so a session that flips the env var sees a consistent
    /// prompt + schema pair.
    fn input_schema(&self) -> &Value {
        if crate::prompt::background_tasks_disabled() {
            &INPUT_SCHEMA_NO_BACKGROUND
        } else {
            &INPUT_SCHEMA
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }

    /// `maxResultSizeChars:30000` on the Bash tool descriptor (2.1.220 BIN off
    /// **235694594**), folded through `M0u` → `min(30000, AKr=50000)` = 30 000.
    ///
    /// Deliberately a STATIC literal, NOT [`bash_max_output_length`]: the
    /// oracle's descriptor field is the constant `30000`, and its `Jst()`
    /// (`BASH_MAX_OUTPUT_LENGTH`) reader appears on the Bash path only as the
    /// `D.length>Jst()` nudge predicate (BIN off 235703218). Raising
    /// `BASH_MAX_OUTPUT_LENGTH` therefore does NOT raise the persistence
    /// threshold in claude-code — and `M0u`'s `AKr` ceiling would have clamped
    /// it to 50 000 even if it did.
    fn persistence_threshold(&self) -> Option<usize> {
        Some(MAX_TOOL_OUTPUT_LENGTH)
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        // claude-code `BashTool.tsx:434-436`: `isConcurrencySafe` delegates to
        // `isReadOnly`.
        self.is_read_only(input)
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // claude-code `BashTool.tsx:437-441`: derive from
        // `checkReadOnlyConstraints(input, commandHasAnyCd(input.command))`,
        // read-only iff `result.behavior === 'allow'`.
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return false;
        };
        let compound_has_cd = crate::read_only::command_has_any_cd(command);
        crate::read_only::check_read_only(command, compound_has_cd).is_read_only()
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    /// BASH-19 — port of the oracle's Bash `userFacingName(input)`
    /// (2.1.238 BIN off **294576071**):
    ///
    /// ```js
    /// userFacingName(e){if(!e)return"Bash";
    ///  if(e.command){let t=Ffr(e.command);if(t)return f0i({file_path:t.filePath,old_string:"x"})}
    ///  return V.CLAUDE_CODE_BASH_SANDBOX_SHOW_INDICATOR&&BY(e)?"SandboxedBash":"Bash"}
    /// ```
    ///
    /// Two of the three arms are ported here:
    /// * a MISSING / non-object input renders the bare tool name (`"Bash"`),
    /// * an input that WILL be sandbox-wrapped renders `"SandboxedBash"` when
    ///   the indicator env var is set.
    ///
    /// The middle arm — `Ffr` (`detectSimulatedSedEdit`) relabelling a
    /// single-command `sed -i 's/…/…/' FILE` as the Edit tool's
    /// `"Update"`/`"Updated plan"` — is NOT ported: it exists to name the
    /// `_simulatedSedEdit` permission surface (BIN off 292490136 builds a
    /// `title:"Edit file"` / `kind:"file-edit-diff"` prompt from `Ffr`'s
    /// `{filePath, pattern, replacement, flags}`), and that prompt shape has no
    /// LingXi counterpart. Emitting the label without the diff surface would
    /// name a real shell execution as a file edit — strictly worse than the
    /// truthful `"Bash"`.
    ///
    /// ENV NOTE: the oracle reads `V.CLAUDE_CODE_BASH_SANDBOX_SHOW_INDICATOR`
    /// with plain JS truthiness (`V.X && BY(e)`), NOT its `isEnvTruthy`
    /// allowlist — so any non-empty value enables it, `"0"` and `"false"`
    /// included. Reproduced exactly here (a `platform_api::env::is_env_truthy` call
    /// would be the wrong predicate). Under LingXi branding the name is
    /// `LINGXI_BASH_SANDBOX_SHOW_INDICATOR`.
    ///
    /// CALL SITE: [`Self::check_permissions`] below titles the sandbox-override
    /// prompt with it (the same role the oracle's prompt renderer gives
    /// `userFacingName`), so it is reachable from the turn loop's permission
    /// gate.
    fn user_facing_name_for_input(&self, input: &Value) -> Option<String> {
        // `if(!e) return "Bash"` — JS falsy input (undefined/null). A non-object
        // is likewise nothing this tool can reason about.
        if !input.is_object() {
            return Some(TOOL_NAME.to_string());
        }
        if bash_sandbox_show_indicator() && self.will_sandbox(input) {
            return Some("SandboxedBash".to_string());
        }
        Some(TOOL_NAME.to_string())
    }

    /// BASH-10 — port of the oracle's Bash `checkPermissions` sandbox-override
    /// arm (2.1.238 BIN off **294577064**; the copy `Run outside of the sandbox`
    /// sits at BIN off **114873408**):
    ///
    /// ```js
    /// async checkPermissions(e,t){let r=await M8n(e,t);
    ///  if(e.dangerouslyDisableSandbox&&r.behavior!=="deny"&&r.behavior!=="ask"
    ///     &&!XXn(r.decisionReason)&&!BY(e)&&BY({...e,dangerouslyDisableSandbox:!1}))
    ///    return{behavior:"ask",decisionReason:{type:"sandboxOverride",reason:"dangerouslyDisableSandbox"},
    ///           message:"Run outside of the sandbox"};
    ///  return r}
    /// ```
    ///
    /// The predicate is SPLIT across the two layers that own its inputs, and the
    /// two halves compose into exactly the oracle's conjunction:
    ///
    /// * `r.behavior!=="deny" && r.behavior!=="ask" && !XXn(r.decisionReason)` —
    ///   a property of the BASE decision, which in LingXi is produced by the
    ///   central gate, not by the tool. The turn loop applies it: it consults
    ///   this hook only when its own resolution is a NON-RULE `Allow`
    ///   (`PermissionResolution::Allow { rule_source: None }`).
    /// * `e.dangerouslyDisableSandbox && !BY(e) && BY({...e, dangerouslyDisableSandbox:false})`
    ///   — "the flag, and only the flag, is what takes this command out of the
    ///   sandbox". That needs the live sandbox runtime config, which the tool
    ///   owns; it is evaluated below.
    ///
    /// Every other arm returns the pre-existing allow-all stub, so a Bash call
    /// without `dangerouslyDisableSandbox` (or with sandboxing off) is
    /// byte-identical to before.
    ///
    /// INERT BY DEFAULT: `sandbox_available` is false and
    /// `SandboxRuntimeConfig::enabled` defaults off in a stock install, so
    /// `will_sandbox` is false for both probes and this never fires. It becomes
    /// live the moment sandboxing is enabled AND unsandboxed commands are
    /// allowed — which is precisely the configuration the oracle guards.
    async fn check_permissions(&self, input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        let dangerously_disable_sandbox = input
            .get("dangerouslyDisableSandbox")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // `!BY(e) && BY({...e, dangerouslyDisableSandbox:!1})`: with the flag the
        // command escapes the sandbox, without it the command would have been
        // wrapped. Both probes read ONE snapshot of the runtime config so a
        // concurrent `/sandbox` toggle cannot split the comparison.
        if dangerously_disable_sandbox
            && !self.will_sandbox(input)
            && self.will_sandbox_ignoring_override(input)
        {
            return PermissionResult::Ask {
                reason: PermissionDecisionReason::SandboxOverride {
                    reason: SandboxOverrideReason::DangerouslyDisableSandbox,
                },
                prompt: PermissionPrompt {
                    // The oracle's prompt renderer titles the request with the
                    // tool's `userFacingName` — BASH-19's hook, called here.
                    title: self
                        .user_facing_name_for_input(input)
                        .unwrap_or_else(|| TOOL_NAME.to_string()),
                    // Byte-locked `message:"Run outside of the sandbox"`.
                    message: SANDBOX_OVERRIDE_ASK_MESSAGE.to_string(),
                    options: Vec::new(),
                },
                pending_classifier_check: None,
                metadata: PermissionMetadata::default(),
            };
        }
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-02 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    /// BASH-18 — port of the oracle's Bash `coerceInput` (`Vmm`, 2.1.238 BIN off
    /// **294485010**; identical in 2.1.220):
    ///
    /// ```js
    /// function Vmm(e){if(!ni(e))return null;let t={...e},r=[];
    ///  if("timeout_ms"in t&&!("timeout"in t)){let n=t.timeout_ms;
    ///   if(typeof n==="number"||typeof n==="string"&&/^\d+$/.test(n))t.timeout=n,r.push("timeout_ms");
    ///   delete t.timeout_ms}
    ///  return r.length?{input:t,shapeClass:r.join(",")}:null}
    /// ```
    ///
    /// Faithful details that are easy to get wrong:
    /// * the rewrite is SKIPPED entirely when `timeout` is already present — the
    ///   stray `timeout_ms` then survives into `safeParse` and (with the schema's
    ///   `additionalProperties:false`) legitimately fails validation;
    /// * a non-coercible `timeout_ms` (a float-shaped string, `true`, an object)
    ///   deletes the key on the COPY but pushes nothing, so `r.length === 0` and
    ///   the whole copy is DISCARDED (`null`) — the raw input, `timeout_ms` and
    ///   all, is what reaches the schema. Returning the pruned copy here would be
    ///   a silent divergence that turns a validation error into a success.
    /// * the value is moved ACROSS AS-IS — a numeric STRING stays a string, which
    ///   the Bash `timeout` schema then rejects/accepts exactly as the oracle's
    ///   `VF(E.number())` coercion does.
    ///
    /// CALL SITE: `orchestrator::turn_loop::dispatch_tool_uses_tracked`, between
    /// the unknown-tool arm and `validate_tool_input_schema` — the same slot the
    /// oracle occupies in `checkPermissionsAndCallTool` (BIN off 294282716).
    fn coerce_input(&self, input: &Value) -> Option<tool_api::tool_trait::CoercedInput> {
        // `ni(e)` — plain-object guard.
        let obj = input.as_object()?;
        if !obj.contains_key("timeout_ms") {
            return None;
        }
        if obj.contains_key("timeout") {
            // The oracle never enters the branch, so `timeout_ms` is NOT deleted
            // and no coercion is reported.
            return None;
        }
        let raw = obj.get("timeout_ms").expect("checked above");
        let coercible = match raw {
            Value::Number(_) => true,
            Value::String(s) => !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()),
            _ => false,
        };
        if !coercible {
            // `r.length === 0` ⇒ the oracle returns `null` and throws its pruned
            // copy away.
            return None;
        }
        // Rebuild in the oracle's key ORDER: `{...e}` keeps the original order,
        // `t.timeout = n` APPENDS `timeout` at the end, `delete t.timeout_ms`
        // drops that key in place. `serde_json` is built with `preserve_order`,
        // so a `remove` + `insert` on a clone would be at the mercy of the map's
        // removal strategy — rebuild explicitly instead.
        let mut coerced = serde_json::Map::with_capacity(obj.len());
        for (k, v) in obj {
            if k != "timeout_ms" {
                coerced.insert(k.clone(), v.clone());
            }
        }
        coerced.insert("timeout".to_string(), raw.clone());
        Some(tool_api::tool_trait::CoercedInput {
            input: Value::Object(coerced),
            shape_class: "timeout_ms".to_string(),
        })
    }

    async fn description(&self, input: &Value, _opts: &DescriptionOptions) -> String {
        // claude-code `BashTool.tsx:426-429`: `return description || 'Run shell
        // command'`. A present-but-empty `description` falls through to the
        // default (JS `||` is falsy on `''`); the command is NOT used.
        match input.get("description").and_then(Value::as_str) {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => "Run shell command".into(),
        }
    }

    async fn prompt(&self, opts: &PromptOptions) -> String {
        // BASH.6/BASH.7 — 1:1 with claude-code
        // `getSimplePrompt(model) = Dh(model) ? qUp(/*CONCISE*/) : TXa(/*VERBOSE*/)`.
        // The `Dh(model)` "simple system prompt" gate (binary @~195159752)
        // selects the SHORT current-gen variant vs the LONG classic one; the
        // session/subagent model is threaded via `PromptOptions::model`, and
        // `None` mirrors the binary's `Dh(undefined)` → LONG. BOTH variants are
        // driven by the live sandbox runtime config on this context (the SHORT
        // `qUp` calls the SAME `yXa()`/`sandbox_section`).
        // (/sandbox) Use the effective config so the prompt reflects the live
        // toggle (the frozen config until `/sandbox` flips the shared cell).
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();
        if tool_api::dh_simple_system_prompt(opts.model.as_deref()) {
            crate::prompt::simple_prompt_concise(&sandbox_runtime, opts.model.as_deref())
        } else {
            crate::prompt::simple_prompt(&sandbox_runtime)
        }
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let cmd = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("missing `command`".into()))?;
        if cmd.is_empty() {
            return Err(ValidationError("`command` must not be empty".into()));
        }
        // claude-code's bash `timeout` schema is `VF(E.number().optional())` with
        // NO `.max()` — the resolver `H5a` accepts any number > 0, so an
        // over-`max` value is honored (the schema `describe` text's "(max
        // 600000)" is advisory only). The prior >max rejection is removed (#4).
        //
        // claude-code `validateInput`: block bare `sleep N` (N>=25) when NOT run
        // in the background — BUT only when `sq()=ct("tengu_amber_sentinel",
        // false)` is enabled (default false → inert, like stock claude-code).
        // The message is byte-identical to the binary; `errorCode: 10` has no
        // Rust surface and is dropped.
        let run_bg = input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // BASH-17 — the oracle's gate is `HSe() && !WA() && !e.run_in_background`
        // (2.1.238 and 2.1.220 alike). The `!WA()` conjunct was missing here:
        // with background tasks disabled the block's own remedy
        // (`run_in_background: true`) does not exist, so the oracle stops
        // blocking rather than dead-ending the model.
        if sleep_block_enabled() && !crate::prompt::background_tasks_disabled() && !run_bg {
            if let Some(pattern) = detect_blocked_sleep_pattern(cmd) {
                return Err(ValidationError(format!(
                    "Blocked: {pattern}. To wait for a condition, use Monitor with an until-loop (e.g. `until <check>; do sleep 2; done`). To wait for a command you started, use run_in_background: true. Do not chain shorter sleeps to work around this block."
                )));
            }
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        use platform_api::sandbox::ProcessCommand as SbxCommand;
        use sandbox::decision::{should_use_sandbox, SandboxDecision};

        // PHASE-2: tool-abort `CancellationToken` threaded in by the streaming
        // executor (a child of `tool_abort`). When the turn is discarded
        // (streaming fallback) or the user interrupts, the executor fires it; the
        // foreground run below races against it and, on cancel, drops the `run`
        // future — `kill_on_drop` SIGKILLs the subprocess — returning `Aborted`
        // so the executor substitutes the synthetic abort block. `None` for every
        // non-streaming caller, in which case the run is awaited directly (no
        // behavior change).
        let cancel = ctx.cancel.clone();

        let cmd_str = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing command".into()))?
            .to_string();
        // CLI-5: snapshot the worktree BEFORE the command runs, when the
        // feature is on and the cwd is inside a repository. Presence of
        // `bash_edit_diff` is the gate — the composition root already answered
        // `y0r`, because the answer needs the settings TIERS.
        //
        // The three per-call conjuncts are the oracle's:
        //   `Sn = y0r(mode) && !run_in_background && !SNt(input)`, and the
        //   snapshot itself additionally requires `!_0r(command)`.
        // * a BACKGROUNDED command returns before it has changed anything, so
        //   there is no "after" to diff against;
        // * a command permission classifies as READ-ONLY cannot have changed a
        //   file, so snapshotting it is pure cost;
        // * a bare `git checkout`/`switch`/… moves the whole worktree, and its
        //   diff would describe the branch rather than the command.
        //
        // Every failure answers `None` and the command proceeds: a diff is a
        // convenience, and a shell command must never fail because git did.
        let edit_diff_eligible = !input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && !self.is_read_only(&input)
            && !crate::bash_edit_diff::is_branch_moving_git(&cmd_str);
        let edit_diff_before = match (self.ctx.bash_edit_diff.as_ref(), ctx.cwd.as_ref()) {
            (Some(setup), Some(cwd)) if edit_diff_eligible => {
                crate::bash_edit_diff::snapshot_before(&setup.shadow_root, cwd).await
            }
            _ => None,
        };

        // 2.1.266 `a0` (`src_163219561.js` @1939367): refuse to start a process
        // while THIS agent's own stop is still completing, so a dying agent
        // cannot leave one running behind it.
        //
        // Upstream returns `rSe()` here — the SAME value its abort path returns
        // (`xDt`: `status:"killed"`, `interrupted:true`) — rather than an error
        // carrying the sentence, and logs the sentence instead. This port keeps
        // that invariant in its own vocabulary: `ToolError::Aborted` is what its
        // cancel path returns, and `turn_loop` maps it to the `"interrupted"`
        // `toolDenialKind`. Synthesizing upstream's `code:145` /
        // "Command aborted before execution" body would mean inventing a result
        // shape that exists nowhere else here, and would report a refusal as a
        // command that ran and failed.
        if let Some(agent_id) = ctx.agent_id {
            if platform_api::agent_processes::is_stop_pending(&agent_id.to_string()) {
                tracing::debug!(
                    "Shell exec refused: agent {agent_id} has a kill pending loop settlement"
                );
                return Err(ToolError::Aborted);
            }
        }
        // claude-code `BashTool.tsx` sends the timeout as `timeout` (ms). Resolve
        // it with the faithful `VF` (numeric-string coercion) + `H5a` (use iff a
        // finite number > 0, else default) logic — no upper clamp/rejection (#4/#5).
        let timeout_ms = resolve_timeout_ms(&input);
        // claude-code gates the explicit-background branch on the same
        // background-tasks-disabled flag that removes the parameter from the
        // schema (`if (Oe === !0 && !It)`), so a call that carries the
        // parameter anyway still runs in the foreground.
        let run_bg = input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && !crate::prompt::background_tasks_disabled();
        // Whether exceeding the timeout may move this command to the background
        // rather than kill it (claude-code `dn = !Dl() && $es(command)`). A
        // command whose first statement starts with `sleep` is excluded: the
        // point of a sleep is to finish, so backgrounding it just hides it.
        let can_auto_background =
            !crate::prompt::background_tasks_disabled() && !first_statement_is_sleep(&cmd_str);
        // BASH.5: optional `dangerouslyDisableSandbox` override.
        let dangerously_disable_sandbox = input
            .get("dangerouslyDisableSandbox")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if cfg!(target_os = "windows") {
            return Err(ToolError::InvalidInput(
                "Bash is not supported on Windows; use PowerShellTool".into(),
            ));
        }

        let started_at = SystemTime::now();
        // claude-code 2.1.238 `call`'s `i = Math.floor(Date.now()/1000)*1000` —
        // the second-truncated call-start stamp the `staleReadFileStateHint`
        // compares on-disk mtimes against.
        let call_start_ms: i64 = started_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_millis() as i64 / 1000) * 1000)
            .unwrap_or(0);
        let request_id = ephemeral_id("bash");
        let hash = cmd_hash(&cmd_str);

        // ===== Start telemetry =====
        let mut meta_start: LogEventMetadata = HashMap::new();
        meta_start.insert(
            "request_id".into(),
            AnalyticsValue::String(request_id.clone()),
        );
        meta_start.insert("_PROTO_command_hash".into(), AnalyticsValue::String(hash));
        meta_start.insert("timeout_ms".into(), AnalyticsValue::Int(timeout_ms as i64));
        meta_start.insert("run_in_background".into(), AnalyticsValue::Bool(run_bg));
        self.ctx.bus.log_event(BASH_STARTED, meta_start).await;

        // ===== Sandbox decision =====
        // 1:1 with claude-code `shouldUseSandbox.ts`. The `dangerouslyDisableSandbox`
        // override and the empty-command / excluded-command short-circuits all live
        // INSIDE `should_use_sandbox` now; we just feed it the inputs.
        // `unsandboxed_allowed` is `SandboxManager.areUnsandboxedCommandsAllowed()`,
        // mapped to the canonical `are_unsandboxed_commands_allowed()` accessor (the
        // same mapping used by the BASH.6 prompt section).
        // (/sandbox) Resolve the effective sandbox config ONCE for this command:
        // the frozen `sandbox_runtime` with `enabled` overridden by the live
        // `/sandbox` toggle cell when wired. This is THE read that makes the
        // toggle affect the wrap decision (`should_use_sandbox` keys off
        // `cfg.enabled`); the excluded-commands / allow-unsandboxed values are
        // unchanged, so they stay consistent with the frozen config.
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();
        // Resolve the session cwd ONCE for this command (worktree parity
        // plan, Task 2): every `workspace`/cwd read below in this call reuses
        // this single snapshot instead of re-reading `self.ctx.cwd()`, so a
        // concurrent swap mid-call can't produce an inconsistent mix of old
        // and new cwd within one command's sandbox-decision/spawn/reset logic.
        let workspace = self.ctx.cwd();
        // ===== Worktree parity plan (Task 4): STATE.cwd re-point =====
        // claude-code's persistent shell cwd moves for free on
        // `EnterWorktree`/`ExitWorktree`, because both call `process.chdir`
        // and `Shell.ts` reads the live process cwd. Our persistent shell cwd
        // (`shell_cwd`, BASH.4 above) is a `BashTool`-local snapshot that does
        // NOT automatically follow `SessionCwd::swap`, so without this a
        // worktree swap would strand subsequent Bash calls in the stale
        // directory. Detect a swap by comparing this call's fresh
        // `ctx.cwd()` (`workspace`) against the session cwd we last observed
        // (`synced_session_cwd`, distinct from `shell_cwd` — see its field
        // doc): a change here can ONLY be a `session_cwd.swap(..)` (nothing
        // else mutates the `SessionCwd` cell), never a model `cd`, so it is
        // safe to re-point `shell_cwd` to the new session cwd. When nothing
        // has swapped, `workspace` always equals `synced_session_cwd`, so this
        // block is a no-op and `shell_cwd` behaves exactly as before
        // (INERT INVARIANT).
        {
            let mut synced = self.synced_session_cwd.lock().unwrap();
            if *synced != workspace {
                *self.shell_cwd.lock().unwrap() = workspace.clone();
                *synced = workspace.clone();
            }
        }
        // ===== BASH.4 persistent cwd, resolved ONCE for BOTH modes =====
        // A subagent isolated in a worktree (`isolation:"worktree"`) or given an
        // explicit `cwd` runs EACH command in that directory, reset per call
        // (claude-code's "Agent threads always have their cwd reset between bash
        // calls"), WITHOUT touching the shared persistent shell cwd (which the
        // main loop owns). The main loop (`ctx.cwd` None) reads the live
        // persistent shell cwd as before.
        //
        // claude-code has ONE shell spawn for both modes (`vV`), so this is
        // derived above the `run_bg` branch and consumed by both arms —
        // including the recovery below, which the background arm used to get
        // for free by being pinned to the workspace.
        let agent_cwd = ctx.cwd.clone();
        let cwd = match &agent_cwd {
            Some(c) => c.clone(),
            None => self.shell_cwd.lock().unwrap().clone(),
        };
        // Deleted-cwd recovery (Shell.ts:220-238): if the live cwd no longer
        // exists on disk (e.g. a prior command deleted its own dir), fall back
        // to the tool workspace (TS `getOriginalCwd`); if that is also gone,
        // fail with the byte-locked message.
        let cwd = if std::fs::canonicalize(&cwd).is_ok() {
            cwd
        } else if std::fs::canonicalize(&workspace).is_ok() {
            // Only the main loop's FOREGROUND path persists the recovered cwd to
            // the shared shell: a subagent's cwd is per-call, and a background
            // launch must never mutate the shared cwd (BASH.4). A background
            // command still RUNS in the recovered directory; it just leaves the
            // repair to the next foreground call.
            if agent_cwd.is_none() && !run_bg {
                self.shell_cwd.lock().unwrap().clone_from(&workspace);
            }
            workspace.clone()
        } else {
            return Err(ToolError::Internal(format!(
                "Working directory \"{}\" no longer exists. Please restart Claude from an existing directory.",
                cwd.display()
            )));
        };

        let decision = should_use_sandbox(
            &cmd_str,
            self.ctx.sandbox_available,
            dangerously_disable_sandbox,
            sandbox_runtime.are_unsandboxed_commands_allowed(),
            &sandbox_runtime,
            workspace.clone(),
        );

        let shell = resolve_shell_path().to_string();

        // BASH.2: defensively rewrite Windows CMD-style `2>nul` redirects to
        // POSIX `/dev/null` before the command is spawned (claude-code
        // `bashProvider.ts` calls `rewriteWindowsNullRedirect(command)` to
        // produce the `normalizedCommand` that is then eval'd).
        //
        // BASH.1 (SECURITY): prepend the extglob-disable prefix so extended
        // globs cannot expand malicious filenames after security validation
        // (claude-code `bashProvider.ts` pushes `getDisableExtglobCommand`
        // before the user command in the `&&`-joined `commandParts`). The
        // prefix is injected INTO the command that becomes `inner_cmd` so it
        // runs in the SAME shell that expands the user's globs — inside the
        // sandbox wrap for the sandbox path, or directly in the login shell for
        // the no-sandbox path — mirroring the TS order `disableExtglob && <cmd>`.
        let normalized_cmd = rewrite_windows_null_redirect(&cmd_str);
        let spawn_cmd = match disable_extglob_command(&shell) {
            Some(prefix) => format!("{prefix} && {normalized_cmd}"),
            None => normalized_cmd,
        };

        let inner_cmd = match decision {
            // HP-6: `sandbox.credentials` must hold even when THIS command is
            // not sandbox-wrapped. `excludedCommands` and
            // `allowUnsandboxedCommands` mean "this command does not need the
            // sandbox", not "this command may read my AWS keys" — and
            // `dangerouslyDisableSandbox` disables the sandbox, not the
            // credential protection the user configured separately. Without
            // this arm the withholding would exist only on the wrapped path,
            // which is a security boundary that silently depends on an
            // unrelated decision.
            SandboxDecision::NoSandbox => format!(
                "{}{}",
                sandbox::credentials::unset_prefix_for_shell(
                    &sandbox_runtime.credential_deny_env,
                    Some(shell.as_str()),
                ),
                spawn_cmd
            ),
            SandboxDecision::Sandbox { policy: _ } => {
                // Wrap through the injected async `SandboxRunner`. The default
                // `LegacyWrapRunner` forwards straight to the sync
                // `wrap_with_sandbox` (ignoring `bin_shell`/`cwd`), so this is
                // byte-identical to the previous direct call; a live runner uses
                // the shell + workspace cwd to scope the sandbox.
                match self
                    .ctx
                    .sandbox_runner
                    .wrap(
                        &spawn_cmd,
                        &sandbox_runtime,
                        self.ctx.platform,
                        Some(&shell),
                        Some(workspace.as_path()),
                    )
                    .await
                {
                    Ok(wrapped) => wrapped,
                    Err(sandbox::wrap::SandboxWrapError::Unsupported(s)) => {
                        emit_failed(&self.ctx.bus, &request_id, "sandbox_refused", started_at)
                            .await;
                        return Err(ToolError::InvalidInput(s));
                    }
                    Err(sandbox::wrap::SandboxWrapError::SbplWrite(s)) => {
                        emit_failed(
                            &self.ctx.bus,
                            &request_id,
                            "sandbox_wrap_failed",
                            started_at,
                        )
                        .await;
                        return Err(ToolError::Io(s));
                    }
                }
            }
        };

        // ===== Background path (TS `!result.backgroundTaskId`) =====
        // Background tasks never mutate the shared cwd, so they skip the
        // `pwd -P` readback entirely — but they START in it, same as the
        // foreground arm (see the shared `cwd` derivation above).
        if run_bg {
            // bg-08: the oracle emits this the moment an explicit background
            // launch is taken (`i("tengu_bash_command_explicitly_backgrounded",
            // {command_type:Npe(ve)})`, `src_160988549.js` @4343453).
            {
                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert(
                    "command_type".into(),
                    AnalyticsValue::String(telemetry_command_type(&cmd_str).into()),
                );
                self.ctx
                    .bus
                    .log_event(BASH_EXPLICITLY_BACKGROUNDED, meta)
                    .await;
            }
            let pcmd = SbxCommand {
                command: shell,
                // BASH.4: login-shell init (see the foreground site) — `-l`
                // after `-c`, matching `bashProvider.ts:201-205` with the
                // snapshot path deferred.
                args: vec!["-c".into(), "-l".into(), inner_cmd],
                // The PERSISTENT shell cwd, the SAME binding the foreground
                // arm uses. claude-code has ONE spawn for both modes (`vV`);
                // `run_in_background` only decides what happens AFTER the shell
                // is already running in the session cwd, so a `cd sub` from an
                // earlier foreground call must be visible here too. Spawning in
                // the workspace instead made a backgrounded command silently
                // run somewhere else than the command before it.
                cwd: Some(cwd.clone()),
                env: HashMap::new(),
                timeout: Some(Duration::from_millis(timeout_ms)),
                stdin: None,
            };
            let sandboxed = self.ctx.sandbox.bypass_with_audit(pcmd, "bash_tool_call");
            // Mint the task identity BEFORE spawning, so the id handed to the
            // model, the record the registry keeps and the file the child
            // writes to are one identity (claude-code `vV` mints it in the one
            // shell spawn; `Xne` then registers that same id). Without a wired
            // registry the runner keeps minting its own — the previous
            // behaviour, retained for hosts that have no task registry.
            let bound = self.allocate_task_identity().await;
            let sandboxed = self.bind_identity(sandboxed, bound.as_ref(), None);
            let sandboxed = sandboxed.with_process_owner(ctx.agent_id.map(|id| id.to_string()));
            return match self.ctx.process.spawn_background(&sandboxed).await {
                Ok(handle) => {
                    // Only claim the registry identity if the runner actually
                    // honoured it; a runner that mints its own id is writing to
                    // its own file, so the registry must not advertise a path
                    // nothing writes to.
                    let bound = self.take_honoured_identity(bound, &handle).await;
                    let task_id = self
                        .acknowledge_background_task(
                            &ctx,
                            bound.as_ref(),
                            &cmd_str,
                            input.get("description").and_then(Value::as_str),
                            &cwd,
                            &handle,
                        )
                        .await?;
                    let out_path = match bound.as_ref() {
                        Some((_, path)) => path.clone(),
                        None => task_output_path(&handle.task_id).display().to_string(),
                    };
                    // Model-facing background note (`y` in the binary's mapper,
                    // built by `$2t`): `Command running in background with ID: …
                    // Output is being written to: … use Read on that file path.`
                    // (offset 183106320), with the 2.1.238 lifetime sentence
                    // selected by `reapedAtFinalResponse`.
                    let reaped = background_ends_with_final_response(&ctx);
                    let mut note = background_note(&task_id, &out_path, None, reaped);
                    // PARITY 2.1.210 (`backgroundCwdHint`): when the backgrounded
                    // command contains a statement-level `cd`/`pushd`/`popd`/`chdir`
                    // (`ror`), the binary appends this hint on a new line so the
                    // model does not assume the directory change took effect for
                    // subsequent commands (the bg run never mutates the session cwd).
                    if crate::read_only::command_has_statement_level_cd(&cmd_str) {
                        note.push_str(&format!(
                            "\nSession cwd remains {}; directory changes made by the backgrounded command do not apply to subsequent commands.",
                            cwd.display()
                        ));
                    }
                    let model_content = bash_model_content("", "", false, Some(&note));
                    Ok(ToolCallResult {
                        // Backgrounded launch: the binary's result data is the
                        // main shape with empty stdout/stderr + `backgroundTaskId`
                        // (the ID + output path ride in the model note, NOT data).
                        data: bash_result_data(
                            "",
                            "",
                            false,
                            false,
                            None,
                            crate::silent::is_silent_bash_command(&cmd_str),
                            Some(&task_id),
                            None,
                            None,
                            reaped,
                        ),
                        model_content: Some(model_content),
                        new_messages: vec![],
                        context_modifier: None,
                        is_error: false,
                        mcp_meta: None,
                    })
                }
                Err(e) => {
                    // The identity was allocated before spawning. Retire its
                    // pending receipt and spool if no task row was published.
                    if let (Some((task_id, _)), Some(registry)) =
                        (bound.as_ref(), self.ctx.task_registry.as_ref())
                    {
                        let _ = registry.settle_background_bash(task_id, None, false).await;
                        registry.discard_bash_output(task_id).await;
                    }
                    emit_failed(&self.ctx.bus, &request_id, "spawn_failed", started_at).await;
                    Err(ToolError::Io(format!("{e}")))
                }
            };
        }

        // Internal cwd-tracking temp file (not model-facing): the shell writes
        // its physical cwd here via `pwd -P` once the user command succeeds.
        let cwd_file = std::env::temp_dir().join(format!("claude-{request_id}-cwd"));
        let q = format!(
            "'{}'",
            cwd_file.display().to_string().replace('\'', "'\\''")
        );
        // Append the readback after the (possibly sandbox-wrapped) command
        // (bashProvider.ts:185-187). `&&` skips the readback when the user
        // command fails (cwd left unchanged — correct); `>|` overrides
        // noclobber and is valid in both bash and zsh.
        let fg_cmd = format!("{inner_cmd} && pwd -P >| {q}");

        let pcmd = SbxCommand {
            command: shell,
            // BASH.4: login-shell init so the shell is "initialized from the
            // user's profile" (the prompt's claim). 1:1 with `bashProvider.ts`
            // `getSpawnArgs` (`:201-205`): `['-c', ...(skipLoginShell ? [] :
            // ['-l']), cmd]`. TS skips `-l` only when a shell SNAPSHOT exists;
            // the snapshot mechanism is deferred here, so `lastSnapshotFilePath`
            // is always undefined ⇒ `skipLoginShell == false` ⇒ `-l` always
            // added (TS's no-snapshot login-shell fallback, `:88-93`).
            args: vec!["-c".into(), "-l".into(), fg_cmd],
            cwd: Some(cwd.clone()),
            env: HashMap::new(),
            timeout: Some(Duration::from_millis(timeout_ms)),
            stdin: None,
        };
        let sandboxed = self
            .ctx
            .sandbox
            .bypass_with_audit(pcmd, "bash_tool_call")
            .with_auto_background_on_timeout(can_auto_background)
            .with_process_owner(ctx.agent_id.map(|id| id.to_string()));
        // A foreground command can still be moved to the background when it
        // exceeds its timeout (claude-code 2.1.210 `timedOutAfterMs`), and the
        // id it is then given must be a registry id for the same reason the
        // explicit-background path needs one. Mint it up front — claude-code
        // does the same, minting one identity per shell command in `vV`
        // regardless of whether it is ever backgrounded — and settle it as
        // completed when the command actually finishes in the foreground.
        let fg_bound = self.allocate_task_identity().await;
        // The other end of claude-code `I_t`'s `t.background(e)`. Ctrl+B,
        // background-all and the SDK `background_tasks` request all reach a
        // still-running foreground shell through this, and the runner treats it
        // exactly like the deadline firing.
        let on_demand = fg_bound
            .as_ref()
            .filter(|_| self.ctx.process.supports_foreground_backgrounding())
            .map(|_| std::sync::Arc::new(tokio::sync::Notify::new()));
        let sandboxed = self.bind_identity(sandboxed, fg_bound.as_ref(), on_demand.clone());
        // claude-code `U6t` after `cnr` (2000) ms: a foreground command that is
        // still running becomes a visible `local_bash` row so `/tasks`, Ctrl+B
        // and background-all have something to address. Short commands never
        // appear — arming at spawn would flash a row for every `ls`.
        let background_reason = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let settled = std::sync::Arc::new(tokio::sync::watch::channel(false).0);
        let _background_settled = BackgroundRequestSettled(settled.clone());
        let arming = match (fg_bound.as_ref(), on_demand.as_ref()) {
            (Some((task_id, _)), Some(notify)) => self.arm_foreground_record(
                &ctx,
                task_id,
                &cmd_str,
                input.get("description").and_then(Value::as_str),
                notify.clone(),
                background_reason.clone(),
                settled,
                _progress_tx.clone(),
            ),
            _ => None,
        };

        // ===== Foreground spawn =====
        // Race the live runner against turn/sibling cancellation. When
        // backgrounding is enabled, preserve the child before returning;
        // forbidden/unbound runners retain the abort-and-kill fallback.
        // PARITY 2.1.210: `run_foreground` moves a timed-out command to the
        // background (returning `MovedToBackground`) rather than killing it; the
        // default trait impl still maps a normal finish to `Completed` and a
        // timeout to `Err(Timeout)`, preserving the interrupted-result fallback.
        let run_fut = self
            .ctx
            .process
            .run_foreground_with_output_limit(&sandboxed, Some(bash_max_output_length()));
        tokio::pin!(run_fut);
        let run_result = match &cancel {
            Some(token) => {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        // `turnAbortBackgrounds: !Dl()` applies even before the
                        // 2 s UI row is armed. Keep polling the same runner so
                        // detachment does not drop (and kill) its OS child.
                        if !crate::prompt::background_tasks_disabled() {
                            if let Some(notify) = &on_demand {
                                let _ = background_reason.compare_exchange(0, platform_api::task_registry::TaskBackgroundReason::TurnAbort as u8, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst);
                                notify.notify_one();
                                run_fut.await
                            } else { return Err(ToolError::Aborted); }
                        } else { return Err(ToolError::Aborted); }
                    }
                    res = &mut run_fut => res,
                }
            }
            None => run_fut.await,
        };
        // The wrapped command has finished: tear down any per-command sandbox
        // state (e.g. bwrap mount points). No-op for the default
        // `LegacyWrapRunner`; only the live runner has anything to clean up.
        self.ctx.sandbox_runner.cleanup_after_command().await;
        let sandbox_violation_lines = self
            .ctx
            .sandbox_runner
            .command_violations(&spawn_cmd)
            .await
            .lines;
        // claude-code surfaces a timed-out/interrupted command as a SUCCESSFUL
        // result with `interrupted: true` plus whatever partial output it
        // produced (`BashTool.tsx` ~602-605 / 720), NOT a hard error. Both
        // timeout arms below build such a result via `build_interrupted_result`.
        // `format_timeout_error` / `BASH_TIMEOUT_ERROR_TEMPLATE` are retained
        // (still referenced by the locked-constant test) but no longer returned.
        // The identity minted before the spawn is only used when the command is
        // moved to the background; on every other outcome its file is redundant,
        // so delete it rather than leaving one file per shell call behind
        // (claude-code `deleteOutputFile` under `outputFileRedundant`).
        let moved_to_background = matches!(
            &run_result,
            Ok(platform_api::ForegroundRunResult {
                outcome: platform_api::ForegroundOutcome::MovedToBackground(_),
                ..
            })
        );
        // claude-code `W6t`: a command that finished in the FOREGROUND gives its
        // armed row back. A command that moved to the background keeps it — it
        // owns a live child now, and the background arm below flips
        // `isBackgrounded` and takes over the record.
        if let Some(arming) = arming {
            if moved_to_background {
                arming.keep_for_background().await;
            } else {
                arming.disarm().await;
            }
        }
        if !moved_to_background {
            if let (Some(registry), Some((task_id, _))) =
                (self.ctx.task_registry.as_ref(), fg_bound.as_ref())
            {
                registry.discard_bash_output(task_id).await;
            }
        }

        match run_result {
            // PARITY 2.1.210: the command exceeded its timeout and the runner
            // moved it to the background instead of killing it. Surface the
            // distinct "…did not complete within its Ns timeout and was moved to
            // the background" note + `backgroundTaskId`/`timedOutAfterMs` (the
            // binary's `l !== void 0` mapper branch), exactly like an explicit
            // background launch except for the message and the extra field.
            Ok(platform_api::ForegroundRunResult {
                outcome: platform_api::ForegroundOutcome::MovedToBackground(handle),
                ..
            }) => {
                let reason = background_reason.load(std::sync::atomic::Ordering::SeqCst);
                let timed_out = (reason == 0).then_some(timeout_ms);
                if reason == 0 {
                    let mut meta: LogEventMetadata = HashMap::new();
                    meta.insert(
                        "request_id".into(),
                        AnalyticsValue::String(request_id.clone()),
                    );
                    meta.insert("timeout_ms".into(), AnalyticsValue::Int(timeout_ms as i64));
                    self.ctx.bus.log_event(BASH_TIMEOUT, meta.clone()).await;
                    // bg-08: the oracle's OWN event for this path
                    // (`wn("tengu_bash_command_timeout_backgrounded",hn)`). It is
                    // additional to the LingXi `tengu_tool_bash_timeout` above,
                    // which has no upstream twin and other tooling reads.
                    let mut bg_meta: LogEventMetadata = HashMap::new();
                    bg_meta.insert(
                        "command_type".into(),
                        AnalyticsValue::String(telemetry_command_type(&cmd_str).into()),
                    );
                    bg_meta.insert("timeout_ms".into(), AnalyticsValue::Int(timeout_ms as i64));
                    self.ctx
                        .bus
                        .log_event(BASH_TIMEOUT_BACKGROUNDED, bg_meta)
                        .await;
                } else if reason
                    == platform_api::task_registry::TaskBackgroundReason::TurnAbort as u8
                {
                    self.ctx
                        .bus
                        .log_event(
                            telemetry::tengu::tool::BASH_TURN_ABORT_BACKGROUNDED,
                            HashMap::from([(
                                "command_type".into(),
                                AnalyticsValue::String(telemetry_command_type(&cmd_str).into()),
                            )]),
                        )
                        .await;
                }
                // The command is now a background task: give the identity we
                // minted up front an actual record, so its id resolves in
                // TaskOutput / TaskStop / TaskList and its completion produces a
                // `<task-notification>` (claude-code `wn` → `Xne` on the
                // timeout path).
                let fg_bound = self.take_honoured_identity(fg_bound, &handle).await;
                let task_id = self
                    .acknowledge_background_task(
                        &ctx,
                        fg_bound.as_ref(),
                        &cmd_str,
                        input.get("description").and_then(Value::as_str),
                        &cwd,
                        &handle,
                    )
                    .await?;
                let out_path = match fg_bound.as_ref() {
                    Some((_, path)) => path.clone(),
                    None => task_output_path(&handle.task_id).display().to_string(),
                };
                // `$2t`'s `timedOutAfterMs !== undefined` arm; the seconds shown
                // are `Math.max(1, Math.round(timeoutMs / 1000))`.
                let reaped = background_ends_with_final_response(&ctx);
                let mut note =
                    background_note_for_reason(&task_id, &out_path, timed_out, reaped, reason);
                // PARITY 2.1.210 (`backgroundCwdHint`): same hint as an explicit
                // background launch — a timed-out-and-backgrounded command whose
                // text contains a statement-level `cd` never mutates the session
                // cwd, so tell the model it remains unchanged (`ror` + mapper's
                // `if(a)_+="\n"+a`).
                if crate::read_only::command_has_statement_level_cd(&cmd_str) {
                    note.push_str(&format!(
                        "\nSession cwd remains {}; directory changes made by the backgrounded command do not apply to subsequent commands.",
                        cwd.display()
                    ));
                }
                let model_content = bash_model_content("", "", false, Some(&note));
                Ok(ToolCallResult {
                    // Same empty main shape as an explicit background launch,
                    // plus `timedOutAfterMs` = the exceeded timeout in ms.
                    data: {
                        let mut data = bash_result_data(
                            "",
                            "",
                            false,
                            false,
                            None,
                            crate::silent::is_silent_bash_command(&cmd_str),
                            Some(&task_id),
                            None,
                            timed_out,
                            reaped,
                        );
                        if reason == platform_api::task_registry::TaskBackgroundReason::User as u8 {
                            data["backgroundedByUser"] = json!(true);
                        }
                        if reason
                            == platform_api::task_registry::TaskBackgroundReason::DeliverMessage
                                as u8
                        {
                            data["backgroundedToDeliverMessage"] = json!(true);
                        }
                        if reason
                            == platform_api::task_registry::TaskBackgroundReason::TurnAbort as u8
                        {
                            data["backgroundedByTurnAbort"] = json!(true);
                        }
                        data
                    },
                    model_content: Some(model_content),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Ok(platform_api::ForegroundRunResult {
                outcome: platform_api::ForegroundOutcome::Completed(out),
                ..
            }) if out.timed_out => {
                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert(
                    "request_id".into(),
                    AnalyticsValue::String(request_id.clone()),
                );
                meta.insert("timeout_ms".into(), AnalyticsValue::Int(timeout_ms as i64));
                self.ctx.bus.log_event(BASH_TIMEOUT, meta).await;
                // The streaming/stub runner may carry partial stdout/stderr
                // captured before the kill — surface it (claude-code attaches
                // whatever the accumulator held).
                Ok(build_interrupted_result(
                    &out.stdout,
                    &out.stderr,
                    &cmd_str,
                    Some(timeout_ms),
                    &sandbox_violation_lines,
                ))
            }
            Ok(platform_api::ForegroundRunResult {
                outcome: platform_api::ForegroundOutcome::Completed(out),
                output_file,
            }) => {
                // BASH.4 cwd readback (Shell.ts:395-419). Subagents must NOT
                // mutate the shared cwd — TS `preventCwdChanges = !isMainThread`.
                // The main thread has no `agent_id`; a subagent call carries one.
                let prevent_cwd_changes = ctx.agent_id.is_some();
                // claude-code `J2n` (the cwd-reset gate): when the readback shows
                // the shell navigated OUTSIDE the allowed dirs — or the
                // `LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR` env forces it — chdir
                // the shell back to the original (workspace) cwd and warn. We do
                // not chdir a real shell (the cwd is reasserted by the next call's
                // `cwd:` spawn arg), so "reset" here = keep `shell_cwd` at the
                // workspace instead of advancing it to the new dir, plus append
                // the `Y2n` warning to stderr below. `Some(workspace)` => a reset
                // warning must be appended; `None` => no reset.
                let mut cwd_reset_warning: Option<std::path::PathBuf> = None;
                if !prevent_cwd_changes {
                    if let Ok(contents) = std::fs::read_to_string(&cwd_file) {
                        let trimmed = contents.trim();
                        if !trimmed.is_empty() {
                            let new_cwd = std::path::PathBuf::from(trimmed);
                            if new_cwd != cwd {
                                if let Ok(canon) = std::fs::canonicalize(&new_cwd) {
                                    // claude-code `J2n` decision (the bash result
                                    // path's `if(l){if(J2n(Fr(t)))h=Y2n("")}`):
                                    //   J2n: `t=Pt()` (cwd after the command =
                                    //   `canon`), `n=Ar()` (original = workspace),
                                    //   reset when `TFo() || (t!==n && !kF(t))`.
                                    // We compare `canon` to the WORKSPACE (not the
                                    // prior `cwd`), so this is correct across
                                    // multiple calls: `is_within_allowed` returns
                                    // true when `canon == workspace` (subsuming
                                    // J2n's `t!==n` equality guard — a `cd` back to
                                    // the workspace is NOT a reset) and true when
                                    // `canon` is inside it (`!kF` false). `kF`'s
                                    // allowed set is the workspace (TS `b$` also
                                    // folds in `additionalWorkingDirectories` — see
                                    // the documented residual; the common
                                    // no-additional-dirs case is faithful).
                                    let force = tfo_maintain_cwd();
                                    let should_reset =
                                        force || !is_within_allowed(&canon, &workspace);
                                    if should_reset {
                                        // RESET (J2n fires): chdir back to original.
                                        // We do NOT advance `shell_cwd` to `canon`;
                                        // reset it to the workspace (TS `x_(n)`).
                                        // Do NOT fire `CwdChanged` — the cwd did not
                                        // really move from the model's view.
                                        // A subagent's cwd is per-call (no shared
                                        // persistent shell) — never mutate it.
                                        if agent_cwd.is_none() {
                                            (*self.shell_cwd.lock().unwrap())
                                                .clone_from(&workspace);
                                        }
                                        // `Y2n` appends `\nShell cwd was reset to
                                        // {Pt()}` where `Pt()` is the cwd AFTER the
                                        // chdir-back == the workspace.
                                        cwd_reset_warning = Some(workspace.clone());
                                        // `j("tengu_bash_tool_reset_to_original_dir")`
                                        // fires ONLY on the non-TFo branch
                                        // (`if(!r)`). It is NOT a registered
                                        // tengu/ALL_EVENT_NAMES const — emit it as
                                        // an inline tracing event so the registry
                                        // stays at 347.
                                        if !force {
                                            tracing::info!(
                                                event = "tengu_bash_tool_reset_to_original_dir"
                                            );
                                        }
                                    } else {
                                        // UPDATE (J2n does not fire): today's
                                        // behavior exactly. Update the persistent
                                        // cwd, then drop the guard BEFORE the
                                        // best-effort hook fire (no mutex held
                                        // across an `.await`). `clone_from` keeps
                                        // `canon` owned for the fire below.
                                        // A subagent's cwd is per-call — do not
                                        // advance the shared persistent shell.
                                        if agent_cwd.is_none() {
                                            (*self.shell_cwd.lock().unwrap()).clone_from(&canon);
                                        }
                                        // BASH.4 `onCwdChangedForHooks(cwd, newCwd)`
                                        // (Shell.ts:409): fire the `CwdChanged` hook
                                        // when the cwd actually moved. We are already
                                        // inside `new_cwd != cwd`, and the firer
                                        // re-asserts `old != new` (claude-code's
                                        // `oldCwd !== newCwd` guard,
                                        // fileChangedWatcher.ts:137). Best-effort:
                                        // a no-op when no firer is registered
                                        // (mobile / plain `new`) or when the hook is
                                        // absent — never breaks the cwd update.
                                        if let Some(firer) = &self.cwd_changed_firer {
                                            firer
                                                .fire(hooks::CwdChangedFire {
                                                    old: cwd.clone(),
                                                    new: canon,
                                                })
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Always clean up the tracking file (Shell.ts:415-419).
                let _ = std::fs::remove_file(&cwd_file);

                let (stdout_clean, ansi_dropped_out) = strip_ansi_count(&out.stdout);
                let (stderr_clean, ansi_dropped_err) = strip_ansi_count(&out.stderr);
                // claude-code `Y2n` (the cwd-reset warning): when `J2n` fired, the
                // bash result path sets `d=Y2n("")` == `"\nShell cwd was reset to
                // {cwd-after-chdir}"` (Y2n's arg is the EMPTY string; `"".trim()`
                // == "" so the warning is exactly a leading-newline sentence).
                //
                // BINARY DIVERGENCE (documented residual): claude-code's BASH tool
                // result `stderr` field is JUST `d` — the command's stderr folds
                // into stdout via the shell (`stderr_length:0` in its
                // `tengu_bash_tool_command_executed`), so there is never any
                // pre-existing stderr to combine with. LingXi surfaces the
                // command's stderr in the result `stderr` field (a PRE-EXISTING
                // design difference, NOT introduced by this finding), so we append
                // the warning to the already-cleaned `stderr_clean` — the only
                // stderr consumer downstream (the image short-circuit and both text
                // result blocks all read this binding). In claude-code's only real
                // case (empty command stderr) this is byte-identical: an empty
                // `stderr_clean` trims to "" giving exactly `\nShell cwd was reset
                // to {workspace}` == `Y2n("")`.
                let stderr_clean = match &cwd_reset_warning {
                    Some(reset_to) => format!(
                        "{}\nShell cwd was reset to {}",
                        stderr_clean.trim(),
                        reset_to.display()
                    ),
                    None => stderr_clean,
                };
                let stderr_clean =
                    append_sandbox_violations(&stderr_clean, &sandbox_violation_lines);
                // Model-facing stdout normalization (claude-code): strip leading
                // whitespace-only lines + trimEnd, then drop outer empty lines.
                let normalized = crate::shared::strip_empty_lines(
                    &crate::shared::normalize_stdout(&stdout_clean),
                );

                // OTEL `claude_code.commit.count` / `claude_code.pull_request.count`
                // (the counter subset of claude-code's `mEo(command, code, output)`,
                // gated on exit 0 inside). Foreground completions only — the
                // background launch returns before this arm, matching CC's
                // `!m.backgroundTaskId` guard. Byte-noop when OTEL is off.
                telemetry::otel::record_git_operation_counters(&cmd_str, out.exit_code);

                // Image-output short-circuit (claude-code `BashTool/utils.ts`
                // `formatOutput`:138-144 + `BashTool.tsx`:785-802): when the
                // model-facing stdout is a base64 `data:image/…;base64,…` URI
                // (matplotlib/screenshot helpers), return it as an IMAGE rather
                // than truncating it as text. Detection runs on `normalized`
                // (= TS `stripEmptyLines(stdout)`); the URI is never truncated
                // (nothing on this path truncates any more). The image
                // rides on `new_messages` via the Rust image contract (mirrors
                // FileRead `read.rs:718-759`); `data` carries `isImage: true` +
                // the `model_content` placeholder. Oversized images are resized
                // before the untruncated URI is handed to the result mapper.
                if is_image_output(&normalized) {
                    // Gate on the binary's FULL `hKn` predicate: data-URI parse
                    // AND magic-byte sniff of the decoded payload (`Wfe`). A
                    // URI whose payload is not a recognized image falls through
                    // to the normal TEXT path — exactly claude's `if(g)` miss.
                    if let Some((image_uri, payload)) = prepare_shell_image_output(&normalized) {
                        let mut meta: LogEventMetadata = HashMap::new();
                        meta.insert("request_id".into(), AnalyticsValue::String(request_id));
                        meta.insert(
                            "exit_code".into(),
                            AnalyticsValue::Int(i64::from(out.exit_code)),
                        );
                        meta.insert(
                            "output_bytes".into(),
                            AnalyticsValue::Int(payload.len() as i64),
                        );
                        let elapsed_ms = SystemTime::now()
                            .duration_since(started_at)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        meta.insert("duration_ms".into(), AnalyticsValue::Int(elapsed_ms as i64));
                        meta.insert(
                            "ansi_chars_stripped".into(),
                            AnalyticsValue::Int((ansi_dropped_out + ansi_dropped_err) as i64),
                        );
                        meta.insert("truncated".into(), AnalyticsValue::Bool(false));
                        // bg-08: `was_backgrounded: Boolean(Se.backgroundTaskId)`
                        // (`src_160988549.js` @4338725). This arm is the
                        // FOREGROUND completion, so the command was never
                        // backgrounded — the field is emitted as `false` rather
                        // than omitted, because "absent" and "ran in the
                        // foreground" are different answers to the query.
                        meta.insert("was_backgrounded".into(), AnalyticsValue::Bool(false));
                        self.ctx.bus.log_event(BASH_COMPLETED, meta).await;

                        let interp = crate::command_semantics::interpret_command_result(
                            &cmd_str,
                            out.exit_code,
                        );
                        return Ok(ToolCallResult {
                            // Image output: the binary's result data is the main
                            // shape with `isImage: true`. `isImage` flags that
                            // `stdout` CONTAINS the image (the data-URI) — the
                            // result mapper (`hKn`, ported as the dispatch loop's
                            // `bash_image_tool_result_blocks`) builds the
                            // tool_result image block FROM `stdout`, with the
                            // media type SNIFFED from the decoded bytes. The
                            // image therefore reaches the model INSIDE the
                            // tool_result content array — no injected message.
                            data: bash_result_data(
                                &image_uri,
                                &stderr_clean,
                                false,
                                true,
                                interp.message.as_deref(),
                                crate::silent::is_silent_bash_command(&cmd_str),
                                None,
                                None,
                                None,
                                false,
                            ),
                            // Display-only (egress ignores it when content_blocks
                            // is Some); claude's wire form has no text.
                            model_content: Some(
                                "[Image content provided in tool result.]".to_string(),
                            ),
                            new_messages: vec![],
                            context_modifier: None,
                            is_error: false,
                            mcp_meta: None,
                        });
                    }
                }

                // no-truncation: A1/STEP-4. The model-facing stdout is NOT
                // truncated — 2.1.220 dropped the shared shell truncator and
                // routes oversized results through the orchestrator's
                // `<persisted-output>` layer keyed on
                // `persistence_threshold()`. `BASH_MAX_OUTPUT_LENGTH` still
                // resolves the boundary (claude-code `Jst()`), but it now only
                // decides whether the result EXCEEDED the limit — the oracle's
                // own `D.length>Jst()` predicate (BIN off 235703218) — which is
                // what the `truncated` analytics field reports.
                let truncated_out = normalized.len() > bash_max_output_length();
                let stdout_final = normalized;
                // Exit-code reinterpretation (claude-code interpretCommandResult):
                // e.g. `grep` no-match (exit 1) is NOT an error.
                let interp =
                    crate::command_semantics::interpret_command_result(&cmd_str, out.exit_code);

                let elapsed_ms = SystemTime::now()
                    .duration_since(started_at)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert("request_id".into(), AnalyticsValue::String(request_id));
                meta.insert(
                    "exit_code".into(),
                    AnalyticsValue::Int(i64::from(out.exit_code)),
                );
                meta.insert(
                    "output_bytes".into(),
                    AnalyticsValue::Int((stdout_final.len() + stderr_clean.len()) as i64),
                );
                meta.insert("duration_ms".into(), AnalyticsValue::Int(elapsed_ms as i64));
                meta.insert(
                    "ansi_chars_stripped".into(),
                    AnalyticsValue::Int((ansi_dropped_out + ansi_dropped_err) as i64),
                );
                meta.insert("truncated".into(), AnalyticsValue::Bool(truncated_out));
                // bg-08: see the image arm above — this is the foreground
                // completion, so `was_backgrounded` is `false`, not absent.
                meta.insert("was_backgrounded".into(), AnalyticsValue::Bool(false));
                self.ctx.bus.log_event(BASH_COMPLETED, meta).await;
                // `staleReadFileStateHint` — computed BEFORE the read-state
                // refresh (oracle order: `te = …OcT(…)` then `await Zmm(…)`),
                // and only on the non-interrupted, non-image, non-background
                // arm (`if(!g&&!J&&!_.backgroundTaskId)`). This arm is exactly
                // that one.
                let stale_hint =
                    stale_read_file_state_hint(&self.ctx, &cmd_str, &cwd, call_start_ms);
                invalidate_written_read_state(&self.ctx, &cwd, &cmd_str);

                // BASH-07 `ghRateLimitHint` (`q` in the oracle's `call`): a
                // non-background `gh` command whose output reports a GitHub API
                // rate-limit error gets the system-reminder appended LAST,
                // matching the mapper's `[h,g,y,p,f].filter(Boolean).join("\n")`
                // order (`p` = staleReadFileStateHint, `f` = this).
                let session_key = self
                    .ctx
                    .session_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let gh_hint = gh_rate_limit_hint(
                    &session_key,
                    &cmd_str,
                    &stdout_final,
                    &stderr_clean,
                    SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                );
                let trailing_notes = [stale_hint.as_deref(), gh_hint]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<&str>>()
                    .join("\n");

                // Model sees the plain-text `[stdout, stderr].join("\n")` render
                // (`content` in the binary's tool_result mapper), NOT the JSON
                // object — which stays for the TUI / PostToolUse hook.
                // CLI-5: the AFTER snapshot, and the diff it implies. Taken
                // here rather than in a `Drop`, so a command that never reached
                // this point (aborted, refused) never pays for it.
                let edit_diff = match edit_diff_before.as_ref() {
                    Some((shadow, before)) => {
                        crate::bash_edit_diff::diff_after(shadow, before).await
                    }
                    None => None,
                };
                let trailing_notes = match edit_diff.as_ref() {
                    Some(diff) => {
                        format!("{trailing_notes}{}", crate::bash_edit_diff::render(diff))
                    }
                    None => trailing_notes,
                };
                let model_content = bash_model_content(
                    &stdout_final,
                    &stderr_clean,
                    false,
                    if trailing_notes.is_empty() {
                        None
                    } else {
                        Some(trailing_notes.as_str())
                    },
                );
                Ok(ToolCallResult {
                    // claude-code 2.1.191 `BashTool` outputSchema (pure metadata).
                    // A completed text command: `interrupted: false`, `isImage:
                    // false`; `returnCodeInterpretation` only when the exit code
                    // carries a semantic meaning (`interp.message`, else omitted).
                    data: bash_result_data(
                        &stdout_final,
                        &stderr_clean,
                        false,
                        false,
                        interp.message.as_deref(),
                        crate::silent::is_silent_bash_command(&cmd_str),
                        None,
                        output_file.as_ref(),
                        None,
                        false,
                    ),
                    model_content: Some(model_content),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Err(platform_api::process::ProcessError::Timeout) => {
                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert(
                    "request_id".into(),
                    AnalyticsValue::String(request_id.clone()),
                );
                meta.insert("timeout_ms".into(), AnalyticsValue::Int(timeout_ms as i64));
                self.ctx.bus.log_event(BASH_TIMEOUT, meta).await;
                // RESIDUAL: the non-streaming posix runner uses
                // `timeout(..).wait_with_output()` and drops captured bytes on
                // kill, so no partial output is available on this arm — emit the
                // correct interrupted shape with empty partial. (The
                // `Ok(timed_out)` arm above DOES carry partial bytes.)
                Ok(build_interrupted_result(
                    "",
                    "",
                    &cmd_str,
                    Some(timeout_ms),
                    &sandbox_violation_lines,
                ))
            }
            Err(e) => {
                emit_failed(&self.ctx.bus, &request_id, "spawn_failed", started_at).await;
                Err(ToolError::Io(format!("{e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::process::ProcessOutput;
    use tool_api::test_support::{fresh_tx, shell_test_ctx};

    fn use_ctx() -> ToolUseContext {
        tool_api::test_support::fresh_ctx()
    }

    /// `ephemeral_id` must never repeat within a process. It used to be
    /// `nanos ^ pid`, and the clock does not advance on every call — two
    /// concurrent Bash calls could share an id, hence share the
    /// `/tmp/claude-<id>-cwd` readback file, and one call's cleanup deleted the
    /// other's `cd` result.
    #[test]
    fn ephemeral_id_is_unique_across_a_tight_loop() {
        let ids: std::collections::HashSet<String> =
            (0..10_000).map(|_| ephemeral_id("bash")).collect();
        assert_eq!(
            ids.len(),
            10_000,
            "ephemeral_id collided within one process"
        );
    }

    #[test]
    fn ephemeral_id_is_unique_across_threads() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let seen = Arc::clone(&seen);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    let id = ephemeral_id("bash");
                    assert!(
                        seen.lock().unwrap().insert(id.clone()),
                        "duplicate ephemeral_id across threads: {id}"
                    );
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        assert_eq!(seen.lock().unwrap().len(), 16_000);
    }

    /// bg-08: `Npe` reports the FIRST KNOWN command name across the whole
    /// command line, not the first sub-command's name — `cd foo && cargo build`
    /// is a `cargo` invocation as far as the backgrounding telemetry is
    /// concerned.
    #[test]
    fn telemetry_command_type_reports_the_first_known_name() {
        assert_eq!(telemetry_command_type("cargo build --release"), "cargo");
        assert_eq!(telemetry_command_type("cd foo && cargo build"), "cargo");
        assert_eq!(telemetry_command_type("ls -la"), "other");
        assert_eq!(telemetry_command_type(""), "other");
        // The match is on the whole first token, not a prefix: `nix-build` and
        // `nix` are separate entries and `cargofmt` is neither.
        assert_eq!(telemetry_command_type("nix-build ."), "nix-build");
        assert_eq!(telemetry_command_type("cargofmt"), "other");
        // `wget` is `Les`'s one addition on top of `yWt`.
        assert_eq!(telemetry_command_type("wget https://x"), "wget");
    }

    #[test]
    fn format_duration_ms_matches_qs() {
        // Sub-minute → floored seconds.
        assert_eq!(format_duration_ms(0), "0s");
        assert_eq!(format_duration_ms(200), "0s");
        assert_eq!(format_duration_ms(5_000), "5s");
        assert_eq!(format_duration_ms(59_999), "59s");
        // Minute+ → largest units down, seconds rounded, 60→carry.
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(120_000), "2m 0s");
        assert_eq!(format_duration_ms(119_600), "2m 0s"); // 1m 59.6s → round → 2m 0s
        assert_eq!(format_duration_ms(3_600_000), "1h 0m 0s");
        assert_eq!(format_duration_ms(3_661_000), "1h 1m 1s");
        assert_eq!(format_duration_ms(90_061_000), "1d 1h 1m"); // days form drops seconds
    }

    #[test]
    fn timeout_result_prepends_duration_annotation_to_stderr() {
        let r = build_interrupted_result("out", "boom", "sleep 200", Some(120_000), &[]);
        let mc = r.model_content.unwrap();
        // claude `uqh`: annotation, space, partial stderr — then the abort marker.
        assert!(
            mc.contains("Command timed out after 2m 0s boom"),
            "annotation missing/misplaced: {mc}"
        );
        assert!(
            mc.contains("<error>Command was aborted before completion</error>"),
            "abort marker missing: {mc}"
        );
        // A plain interrupt (no timeout) carries NO duration annotation.
        let plain = build_interrupted_result("out", "boom", "cmd", None, &[]);
        assert!(!plain
            .model_content
            .unwrap()
            .contains("Command timed out after"));
    }

    #[test]
    fn locked_constants_unchanged() {
        assert_eq!(BASH_DEFAULT_TIMEOUT_MS, 120_000);
        assert_eq!(BASH_MAX_TIMEOUT_MS, 600_000);
        assert_eq!(
            BASH_TIMEOUT_ERROR_TEMPLATE,
            "Bash command timed out after {N}ms"
        );
        assert_eq!(BASH_SHELL_LINUX, "/bin/bash");
        assert_eq!(BASH_SHELL_MACOS, "/bin/zsh");
        assert_eq!(TOOL_NAME, "Bash");
    }

    #[test]
    fn model_content_matches_binary_join() {
        // [c, u, d].filter(Boolean).join("\n"): stdout + stderr joined by newline.
        assert_eq!(
            bash_model_content("hello\n", "warn: x\n", false, None),
            "hello\nwarn: x"
        );
        // stdout only (stderr empty ⇒ dropped, no trailing newline).
        assert_eq!(bash_model_content("ok\n", "", false, None), "ok");
        // stderr only (empty stdout ⇒ dropped).
        assert_eq!(bash_model_content("", "boom", false, None), "boom");
        // Leading blank lines stripped + trimEnd (normalize_stdout / the `c` rule).
        assert_eq!(
            bash_model_content("\n\n  data  \n", "", false, None),
            "  data"
        );
        // Interrupt appends the abort marker after a newline (rYa) when stderr present.
        assert_eq!(
            bash_model_content("partial\n", "err", true, None),
            format!("partial\nerr\n{ABORT_MARKER}")
        );
        // Interrupt with empty stderr ⇒ marker only (no leading newline).
        assert_eq!(
            bash_model_content("", "", true, None),
            ABORT_MARKER.to_string()
        );
        // Background note is the `d` part.
        assert_eq!(
            bash_model_content(
                "",
                "",
                false,
                Some("Command running in background with ID: 7. Output is being written to: /p.")
            ),
            "Command running in background with ID: 7. Output is being written to: /p."
        );
    }

    // ===== Finding #8 — `R0` containment port (`is_within_allowed`) ==========

    #[test]
    fn normalize_private_prefix_folds_var_and_tmp() {
        use std::path::Path;
        // `/private/var/...` → `/var/...`
        assert_eq!(
            normalize_private_prefix(Path::new("/private/var/folders/ab/x")),
            "/var/folders/ab/x"
        );
        // `/private/tmp/...` → `/tmp/...` (the `$1` keeps the trailing segment)
        assert_eq!(
            normalize_private_prefix(Path::new("/private/tmp/foo")),
            "/tmp/foo"
        );
        // `/private/tmp` (end-of-string) → `/tmp`
        assert_eq!(normalize_private_prefix(Path::new("/private/tmp")), "/tmp");
        // Non-matching prefixes pass through unchanged (no `/private/varX` fold).
        assert_eq!(
            normalize_private_prefix(Path::new("/private/variable")),
            "/private/variable"
        );
        assert_eq!(normalize_private_prefix(Path::new("/usr/bin")), "/usr/bin");
    }

    #[test]
    fn is_within_allowed_same_dir_is_contained() {
        // `R0`: `posix.relative(t, e) == ""` ⇒ same dir ⇒ contained.
        let dir = std::env::temp_dir();
        let canon = std::fs::canonicalize(&dir).unwrap();
        assert!(is_within_allowed(&canon, &canon));
    }

    #[test]
    fn is_within_allowed_subdir_is_contained() {
        // A real subdir of a real (canonicalized) workspace is contained — the
        // `/private/var` realpath fold on macOS must not break the prefix check.
        let ws = tempfile::TempDir::new().unwrap();
        let ws_canon = std::fs::canonicalize(ws.path()).unwrap();
        let sub = ws_canon.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let sub_canon = std::fs::canonicalize(&sub).unwrap();
        assert!(
            is_within_allowed(&sub_canon, &ws_canon),
            "{} should be within {}",
            sub_canon.display(),
            ws_canon.display()
        );
    }

    #[test]
    fn is_within_allowed_sibling_is_not_contained() {
        // `R0`: `posix.relative(t, e)` is `..`-leading (or absolute) ⇒ outside.
        // Two independent temp dirs are siblings, never nested.
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        let a_canon = std::fs::canonicalize(a.path()).unwrap();
        let b_canon = std::fs::canonicalize(b.path()).unwrap();
        assert!(
            !is_within_allowed(&b_canon, &a_canon),
            "{} must NOT be within {}",
            b_canon.display(),
            a_canon.display()
        );
    }

    #[test]
    fn is_within_allowed_prefix_string_not_path_boundary() {
        // `/var/foobar` is NOT inside `/var/foo` — the boundary must be a path
        // separator, mirroring `posix.relative` (`relative('/var/foo','/var/foobar')`
        // == `../foobar`, `..`-leading ⇒ not contained). Build real dirs whose
        // canonical names share a string prefix but are siblings.
        let root = tempfile::TempDir::new().unwrap();
        let root_canon = std::fs::canonicalize(root.path()).unwrap();
        let foo = root_canon.join("foo");
        let foobar = root_canon.join("foobar");
        std::fs::create_dir(&foo).unwrap();
        std::fs::create_dir(&foobar).unwrap();
        assert!(
            !is_within_allowed(&foobar, &foo),
            "{} must NOT be 'within' {} (string-prefix, not path-prefix)",
            foobar.display(),
            foo.display()
        );
    }

    #[test]
    fn format_timeout_error_substitutes_n() {
        assert_eq!(
            format_timeout_error(200),
            "Bash command timed out after 200ms"
        );
        assert_eq!(
            format_timeout_error(120_000),
            "Bash command timed out after 120000ms"
        );
    }

    /// Serializes every test that mutates `LINGXI_SHELL`.
    ///
    /// `std::env::set_var` is PROCESS-GLOBAL, and these four tests each set the
    /// variable, read it back, and clear it. Run in parallel they overwrite one
    /// another — the bash test read `/usr/local/bin/zsh` because the zsh test
    /// had just set it. It surfaced only under a saturated full-workspace run
    /// and passed every time in isolation, which is exactly how it survived.
    static SHELL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Poison-tolerant: the payload is `()`, so a panicking test leaves nothing
    /// to corrupt and must not wedge the rest of the family.
    fn shell_env_lock() -> std::sync::MutexGuard<'static, ()> {
        SHELL_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn resolve_shell_path_matches_host_os() {
        let _g = shell_env_lock();
        // Ensure LINGXI_SHELL is unset so we hit the OS-fallback branch.
        std::env::remove_var("LINGXI_SHELL");
        if cfg!(target_os = "macos") {
            assert_eq!(resolve_shell_path(), "/bin/zsh");
        } else {
            assert_eq!(resolve_shell_path(), "/bin/bash");
        }
    }

    /// LINGXI_SHELL override: a bash path is honoured.
    #[test]
    fn resolve_shell_path_honours_claude_code_shell_bash() {
        let _g = shell_env_lock();
        std::env::set_var("LINGXI_SHELL", "/opt/homebrew/bin/bash");
        let result = resolve_shell_path();
        std::env::remove_var("LINGXI_SHELL");
        assert_eq!(result, "/opt/homebrew/bin/bash");
    }

    /// LINGXI_SHELL override: a zsh path is honoured.
    #[test]
    fn resolve_shell_path_honours_claude_code_shell_zsh() {
        let _g = shell_env_lock();
        std::env::set_var("LINGXI_SHELL", "/usr/local/bin/zsh");
        let result = resolve_shell_path();
        std::env::remove_var("LINGXI_SHELL");
        assert_eq!(result, "/usr/local/bin/zsh");
    }

    /// LINGXI_SHELL set to an unsupported shell (neither bash nor zsh)
    /// falls back to the OS default — matches TS fallback path.
    #[test]
    fn resolve_shell_path_rejects_unsupported_shell() {
        let _g = shell_env_lock();
        std::env::set_var("LINGXI_SHELL", "/bin/sh");
        std::env::remove_var("LINGXI_SHELL"); // first clear; now test with fish
        std::env::set_var("LINGXI_SHELL", "/usr/bin/fish");
        let result = resolve_shell_path();
        std::env::remove_var("LINGXI_SHELL");
        // fish contains neither "bash" nor "zsh", so OS default is used.
        if cfg!(target_os = "macos") {
            assert_eq!(result, "/bin/zsh");
        } else {
            assert_eq!(result, "/bin/bash");
        }
    }

    /// Empty LINGXI_SHELL falls back to OS default.
    #[test]
    fn resolve_shell_path_ignores_empty_claude_code_shell() {
        let _g = shell_env_lock();
        std::env::set_var("LINGXI_SHELL", "");
        let result = resolve_shell_path();
        std::env::remove_var("LINGXI_SHELL");
        if cfg!(target_os = "macos") {
            assert_eq!(result, "/bin/zsh");
        } else {
            assert_eq!(result, "/bin/bash");
        }
    }

    // ---- Git Bash resolution (cc 2.1.219 `MQ`/`P6n`) -----------------------

    #[test]
    fn git_bash_classifier_matrix() {
        let all_exist = |_: &str| true;
        let none_exist = |_: &str| false;
        // Valid basenames, case-insensitive, both separators.
        for p in [
            r"C:\tools\bash.exe",
            r"C:\tools\BASH.EXE",
            "C:/git/bin/sh.exe",
            r"D:\x\bash",
            "/usr/bin/sh",
        ] {
            assert_eq!(
                classify_git_bash_override(p, &all_exist),
                GitBashOverride::Valid,
                "{p}"
            );
            assert_eq!(
                classify_git_bash_override(p, &none_exist),
                GitBashOverride::NotFound,
                "{p}"
            );
        }
        // Invalid basenames never probe existence (`o && e(v)` short-circuit).
        let must_not_probe = |p: &str| -> bool { panic!("existence probed for {p}") };
        for p in [r"C:\tools\pwsh.exe", r"C:\Git\bin\bash.exe.bak", "cmd.exe"] {
            assert_eq!(
                classify_git_bash_override(p, &must_not_probe),
                GitBashOverride::NotBashBinary,
                "{p}"
            );
        }
    }

    #[test]
    fn git_bash_warning_is_byte_exact() {
        // `CLAUDE_CODE_GIT_BASH_PATH "${v}" ${o?"not found":"is not a bash/sh
        // binary"}; falling back to auto-detection`
        assert_eq!(
            git_bash_override_warning(
                "CLAUDE_CODE_GIT_BASH_PATH",
                r"C:\x\bash.exe",
                GitBashOverride::NotFound
            ),
            "CLAUDE_CODE_GIT_BASH_PATH \"C:\\x\\bash.exe\" not found; falling back to auto-detection"
        );
        assert_eq!(
            git_bash_override_warning(
                "CLAUDE_CODE_GIT_BASH_PATH",
                r"C:\x\pwsh.exe",
                GitBashOverride::NotBashBinary
            ),
            "CLAUDE_CODE_GIT_BASH_PATH \"C:\\x\\pwsh.exe\" is not a bash/sh binary; falling back to auto-detection"
        );
    }

    #[test]
    fn git_bash_resolution_order() {
        // 1) Valid override wins verbatim.
        let exists_override = |p: &str| p == r"D:\portable\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(
                Some(("CLAUDE_CODE_GIT_BASH_PATH", r"D:\portable\bash.exe")),
                &exists_override,
                &|| None,
            )
            .as_deref(),
            Some(r"D:\portable\bash.exe")
        );
        // 2) Invalid override falls back to the Program Files probes.
        let exists_pf = |p: &str| p == r"C:\Program Files\Git\bin\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(
                Some(("CLAUDE_CODE_GIT_BASH_PATH", r"D:\missing\bash.exe")),
                &exists_pf,
                &|| None,
            )
            .as_deref(),
            Some(r"C:\Program Files\Git\bin\bash.exe")
        );
        // 3) (x86) probe is second.
        let exists_x86 = |p: &str| p == r"C:\Program Files (x86)\Git\bin\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(None, &exists_x86, &|| None).as_deref(),
            Some(r"C:\Program Files (x86)\Git\bin\bash.exe")
        );
        // 4) git-on-PATH: join(git, "..", "..", "bin", "bash.exe"). The
        // expectation is the LITERAL normalized path, not a recomputation of
        // the code under test — `path.win32.join` collapses the two `..`, so
        // no `..` may survive into the returned string.
        let git = std::path::PathBuf::from(r"C:\Custom\Git\cmd\git.exe");
        let exists_git = |p: &str| p == r"C:\Custom\Git\bin\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(None, &exists_git, &|| Some(git.clone())).as_deref(),
            Some(r"C:\Custom\Git\bin\bash.exe")
        );
        // 5) Nothing anywhere -> None.
        assert_eq!(resolve_git_bash_path_with(None, &|_| false, &|| None), None);
    }

    /// `WMe.join` is `path/win32`'s (@226607353), which normalizes. The result
    /// is exported as `SHELL` and printed by the `Using bash path:` line, so a
    /// surviving `..` is byte-drift on three surfaces at once.
    #[test]
    fn git_bash_candidate_is_win32_normalized() {
        assert_eq!(
            git_bash_beside_git(r"C:\Custom\Git\cmd\git.exe"),
            r"C:\Custom\Git\bin\bash.exe"
        );
        assert!(!git_bash_beside_git(r"C:\Custom\Git\cmd\git.exe").contains(".."));
        // Forward slashes in a PATH entry still render as backslashes.
        assert_eq!(
            git_bash_beside_git("C:/Custom/Git/cmd/git.exe"),
            r"C:\Custom\Git\bin\bash.exe"
        );
        // `..` cannot climb past the drive root.
        assert_eq!(git_bash_beside_git(r"C:\Git\git.exe"), r"C:\bin\bash.exe");
        // UNC share root is preserved.
        assert_eq!(
            git_bash_beside_git(r"\\srv\share\Git\cmd\git.exe"),
            r"\\srv\share\Git\bin\bash.exe"
        );
        // Relative PATH entry keeps the leftover `..` (Node's normalizeString).
        assert_eq!(git_bash_beside_git(r".\git.exe"), r"..\bin\bash.exe");
    }

    #[test]
    fn task_output_path_embeds_task_id() {
        let p = task_output_path("abc123");
        let s = p.display().to_string();
        assert!(s.contains("abc123"), "got {s}");
        assert!(
            std::path::Path::new(&s)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("out")),
            "got {s}",
        );
    }

    #[tokio::test]
    async fn foreground_zero_exit_returns_stdout_and_is_error_false() {
        let out = ProcessOutput {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "echo hello"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        // claude-code normalizes model-facing stdout (trimEnd + stripEmptyLines),
        // so the trailing newline is dropped.
        assert_eq!(res.data["stdout"], "hello");
        assert_eq!(res.data["interrupted"], false);
        assert_eq!(res.data["isImage"], false);
        // `exit_code`/`is_error`/`timed_out` are telemetry-only — NOT result data.
        assert!(res.data.get("exit_code").is_none());
        assert!(res.data.get("is_error").is_none());
        assert!(res.data.get("timed_out").is_none());
        // exit 0 → no semantic interpretation → field omitted (binary `p?.message`).
        assert!(res.data.get("returnCodeInterpretation").is_none());
    }

    /// Worktree parity plan (Task 2) INERT INVARIANT: `BuiltinToolContext`
    /// now carries `session_cwd: Arc<SessionCwd>` instead of frozen
    /// `workspace`/`trusted_dirs` fields; `BashTool::new` seeds `shell_cwd`
    /// from `ctx.cwd()` and the sandbox-decision/spawn/reset logic all read a
    /// single per-call `self.ctx.cwd()` snapshot. With nothing ever calling
    /// `session_cwd.swap(..)`, `ctx.cwd()` must equal the boot cwd
    /// `shell_test_ctx` constructed, and a command must run exactly as it did
    /// before the migration.
    #[tokio::test]
    async fn no_swap_is_identical() {
        let out = ProcessOutput {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let ctx = shell_test_ctx(out);

        // No `session_cwd.swap(..)` call anywhere in this test.
        assert_eq!(ctx.cwd(), std::path::PathBuf::from("/tmp"));
        assert_eq!(ctx.trusted_dirs(), vec![std::path::PathBuf::from("/tmp")]);

        let tool = BashTool::new(ctx);
        let res = tool
            .call(json!({"command": "echo hello"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        assert_eq!(res.data["stdout"], "hello");
        assert_eq!(res.data["interrupted"], false);
    }

    /// A `ProcessRunner` stub that records the `cwd` field of every spawned
    /// [`ProcessCommand`](platform_api::sandbox::ProcessCommand), so a test can
    /// assert which directory the (possibly sandbox-wrapped) foreground
    /// command was actually spawned in.
    struct CwdCapturingRunner {
        out: ProcessOutput,
        last_cwd: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    }
    #[async_trait]
    impl platform_api::process::ProcessRunner for CwdCapturingRunner {
        async fn run(
            &self,
            cmd: &platform_api::sandbox::SandboxedCommand,
        ) -> Result<ProcessOutput, platform_api::process::ProcessError> {
            *self.last_cwd.lock().unwrap() = cmd.inner().cwd.clone();
            Ok(self.out.clone())
        }
        async fn spawn_background(
            &self,
            _: &platform_api::sandbox::SandboxedCommand,
        ) -> Result<platform_api::process::ProcessHandle, platform_api::process::ProcessError>
        {
            unreachable!()
        }
        async fn kill(
            &self,
            _: &platform_api::process::ProcessHandle,
        ) -> Result<(), platform_api::process::ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// Worktree parity plan (Task 4) — the persistent shell cwd (`STATE.cwd`)
    /// must follow a `SessionCwd::swap` the way claude-code's `process.chdir`
    /// moves its shell for free: `EnterWorktree` swaps `session_cwd` to the
    /// worktree, and the NEXT Bash call must spawn there — not in the stale
    /// pre-swap directory the `BashTool` was constructed with. Swapping back
    /// (`ExitWorktree`) must restore the original.
    #[tokio::test]
    async fn bash_cwd_follows_session_cwd_swap() {
        let origin = tempfile::TempDir::new().unwrap();
        let worktree = tempfile::TempDir::new().unwrap();
        let origin_path = origin.path().to_path_buf();
        let worktree_path = worktree.path().to_path_buf();

        let last_cwd = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut ctx = shell_test_ctx(ok_output());
        ctx.session_cwd =
            tool_api::session_cwd::SessionCwd::new(origin_path.clone(), vec![origin_path.clone()]);
        ctx.process = std::sync::Arc::new(CwdCapturingRunner {
            out: ok_output(),
            last_cwd: last_cwd.clone(),
        });
        let session_cwd = ctx.session_cwd.clone();
        let tool = BashTool::new(ctx);

        // Boot: no swap yet — the command spawns in the origin.
        tool.call(json!({"command": "pwd"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        assert_eq!(last_cwd.lock().unwrap().clone(), Some(origin_path.clone()));

        // `EnterWorktree` swaps the session cwd — the persistent shell must
        // follow on the very next call, with no intervening `cd`.
        session_cwd.swap(worktree_path.clone(), vec![worktree_path.clone()]);
        tool.call(json!({"command": "pwd"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        assert_eq!(
            last_cwd.lock().unwrap().clone(),
            Some(worktree_path.clone()),
            "shell cwd must re-point to the swapped-in worktree"
        );

        // `ExitWorktree` swaps back — the persistent shell must restore.
        session_cwd.swap(origin_path.clone(), vec![origin_path.clone()]);
        tool.call(json!({"command": "pwd"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        assert_eq!(
            last_cwd.lock().unwrap().clone(),
            Some(origin_path),
            "shell cwd must restore to the origin after swap-back"
        );
    }

    /// Companion INERT check for Task 4 specifically (distinct from the
    /// Task 2 `no_swap_is_identical` test above): with no `session_cwd.swap`
    /// at all, a `cd` the model runs must still persist across calls exactly
    /// as before the STATE.cwd re-point logic was added — the re-point check
    /// must be a true no-op when `ctx.cwd()` never changes.
    #[tokio::test]
    async fn bash_cd_persists_across_calls_with_no_session_swap() {
        let base = tempfile::TempDir::new().unwrap();
        let sub = base.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let base_path = base.path().to_path_buf();

        let last_cwd = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut ctx = shell_test_ctx(ok_output());
        ctx.session_cwd =
            tool_api::session_cwd::SessionCwd::new(base_path.clone(), vec![base_path.clone()]);
        ctx.process = std::sync::Arc::new(CwdCapturingRunner {
            out: ok_output(),
            last_cwd: last_cwd.clone(),
        });
        let tool = BashTool::new(ctx);

        // First call spawns in the boot cwd.
        tool.call(json!({"command": "pwd"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        assert_eq!(last_cwd.lock().unwrap().clone(), Some(base_path.clone()));

        // A `cd` moves the persistent shell cwd via the `pwd -P` readback
        // (BASH.4); the stub runner doesn't actually create the tracking
        // file, so simulate the readback directly by writing to the temp file
        // the tool expects — this test only needs to prove the STATE.cwd
        // re-point logic (Task 4) does not clobber a plain, unswapped `cd`.
        // (No `session_cwd.swap` is called anywhere in this test.)
        *tool.shell_cwd.lock().unwrap() = sub.clone();

        tool.call(json!({"command": "pwd"}), use_ctx(), fresh_tx())
            .await
            .expect("call should succeed");
        assert_eq!(
            last_cwd.lock().unwrap().clone(),
            Some(sub),
            "an unswapped `cd` must persist untouched by the re-point check"
        );
    }

    /// PHASE-2: when `ctx.cancel` is a token that is already fired, the
    /// foreground run's `tokio::select!` takes the (biased) cancel arm and the
    /// call returns `ToolError::Aborted` — the seam the streaming executor uses
    /// to substitute the synthetic sibling-cancel.
    ///
    /// Determinism: the stub `ProcessRunner::run` returns Ready immediately
    /// (no yield), so BOTH select arms are Ready. We pre-cancel the token AND
    /// use `biased;` with the cancel arm FIRST, so the cancel branch is chosen
    /// deterministically — mirroring real cancellation where the executor fires
    /// the token before/while the subprocess is in flight.
    #[tokio::test]
    async fn foreground_returns_aborted_when_cancel_token_fired() {
        let out = ProcessOutput {
            stdout: "should-not-be-seen\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel(); // pre-fire so `cancelled()` is Ready
        let mut ctx = use_ctx();
        ctx.cancel = Some(token);
        let err = tool
            .call(json!({"command": "sleep 100"}), ctx, fresh_tx())
            .await
            .expect_err("a fired cancel token must abort the run");
        assert!(
            matches!(err, ToolError::Aborted),
            "expected ToolError::Aborted, got {err:?}"
        );
    }

    /// 2.1.266 `a0` (`src_163219561.js` @1939367): a stopping agent must not
    /// start a process that would outlive it.
    ///
    /// Upstream returns `rSe()` — the very value its abort path returns — and
    /// logs the sentence rather than surfacing it. The port's abort value is
    /// `ToolError::Aborted`, which `turn_loop` maps to the `"interrupted"`
    /// `toolDenialKind`; reporting this as a command that ran and failed would
    /// be the wrong shape.
    #[tokio::test]
    async fn a_stopping_agent_cannot_start_a_shell_command() {
        let out = ProcessOutput {
            stdout: "should-not-be-seen\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let mut ctx = use_ctx();
        let agent_id = protocol::AgentId::new();
        ctx.agent_id = Some(agent_id);
        let _stopping = platform_api::agent_processes::mark_stop_pending(&agent_id.to_string());

        let err = tool
            .call(json!({"command": "echo hi"}), ctx, fresh_tx())
            .await
            .expect_err("an agent whose stop is still completing must not exec");
        assert!(
            matches!(err, ToolError::Aborted),
            "the refusal reuses the abort shape, as `rSe()` does upstream; got {err:?}"
        );
    }

    /// Control: the same call with no stop pending runs, so the test above is
    /// pinning the gate rather than a broken fixture.
    #[tokio::test]
    async fn a_shell_command_runs_when_its_agent_is_not_stopping() {
        let out = ProcessOutput {
            stdout: "hi\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let mut ctx = use_ctx();
        ctx.agent_id = Some(protocol::AgentId::new());
        tool.call(json!({"command": "echo hi"}), ctx, fresh_tx())
            .await
            .expect("no stop pending ⇒ the command runs");
    }

    /// Control: with NO cancel token (`ctx.cancel == None`) the run is awaited
    /// directly and completes normally — proving the select wrap is inert for
    /// every non-streaming caller (byte-for-byte unchanged behavior).
    #[tokio::test]
    async fn foreground_no_cancel_token_runs_normally() {
        let out = ProcessOutput {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "echo hello"}), use_ctx(), fresh_tx())
            .await
            .expect("no token → normal completion");
        assert_eq!(res.data["stdout"], "hello");
        assert_eq!(res.data["interrupted"], false);
    }

    #[tokio::test]
    async fn foreground_nonzero_exit_is_ok_data_not_err() {
        let out = ProcessOutput {
            stdout: String::new(),
            stderr: "boom\n".into(),
            exit_code: 7,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "exit 7"}), use_ctx(), fresh_tx())
            .await
            .expect("non-zero exit is data, not Err");
        // A non-zero exit is a SUCCESSFUL result (Ok data), not a hard error.
        // claude-code surfaces no `exit_code`/`is_error` in result data — the
        // failure rides via stderr, the model render, and the non-zero default
        // `returnCodeInterpretation` ("Command failed with exit code N").
        assert!(res.data["stderr"].as_str().unwrap().contains("boom"));
        assert_eq!(res.data["interrupted"], false);
        assert_eq!(
            res.data["returnCodeInterpretation"],
            "Command failed with exit code 7"
        );
        assert!(res.data.get("exit_code").is_none());
        assert!(res.data.get("is_error").is_none());
    }

    #[tokio::test]
    async fn foreground_grep_no_match_carries_return_code_interpretation() {
        // `grep` exit 1 = "no matches": a NON-error carrying a semantic
        // `returnCodeInterpretation` (claude-code `interpretCommandResult`). The
        // field is present (string); there is still no `is_error` in `data`.
        let out = ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 1,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(
                json!({"command": "grep needle file"}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("grep no-match is Ok data");
        assert_eq!(res.data["returnCodeInterpretation"], "No matches found");
        assert_eq!(res.data["interrupted"], false);
        assert!(res.data.get("is_error").is_none());
        assert!(res.data.get("exit_code").is_none());
    }

    #[tokio::test]
    async fn foreground_timed_out_returns_ok_interrupted_with_partial() {
        // claude-code parity: a timeout is a SUCCESSFUL result carrying the
        // partial output + `interrupted: true`, NOT a hard error.
        let out = ProcessOutput {
            stdout: "partial-out\n".into(),
            stderr: String::new(),
            exit_code: -1,
            timed_out: true,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(
                json!({"command": "do-work", "timeout": 200}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("timeout should be Ok(interrupted)");
        assert_eq!(res.data["interrupted"], true);
        assert_eq!(res.data["stdout"], "partial-out");
        assert!(res.data.get("timed_out").is_none());
        assert!(res.data.get("is_error").is_none());
        assert!(
            res.data["stderr"]
                .as_str()
                .unwrap()
                .contains("<error>Command was aborted before completion</error>"),
            "expected abort marker in stderr, got: {:?}",
            res.data["stderr"]
        );
    }

    #[tokio::test]
    async fn foreground_process_timeout_err_returns_ok_interrupted_empty_partial() {
        // The non-streaming runner reports a kill as `Err(ProcessError::Timeout)`
        // and drops captured bytes — still an Ok(interrupted) shape, empty stdout.
        struct TimeoutStub;
        #[async_trait]
        impl platform_api::process::ProcessRunner for TimeoutStub {
            async fn run(
                &self,
                _: &platform_api::sandbox::SandboxedCommand,
            ) -> Result<ProcessOutput, platform_api::process::ProcessError> {
                Err(platform_api::process::ProcessError::Timeout)
            }
            async fn spawn_background(
                &self,
                _: &platform_api::sandbox::SandboxedCommand,
            ) -> Result<platform_api::process::ProcessHandle, platform_api::process::ProcessError>
            {
                unreachable!()
            }
            async fn kill(
                &self,
                _: &platform_api::process::ProcessHandle,
            ) -> Result<(), platform_api::process::ProcessError> {
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = std::sync::Arc::new(TimeoutStub);
        let tool = BashTool::new(ctx);
        let res = tool
            .call(
                json!({"command": "do-work", "timeout": 200}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("process-timeout should be Ok(interrupted)");
        assert_eq!(res.data["interrupted"], true);
        assert_eq!(res.data["stdout"], "");
        assert!(res.data.get("timed_out").is_none());
        assert!(res.data.get("is_error").is_none());
    }

    #[tokio::test]
    async fn foreground_timeout_moved_to_background_gets_distinct_note() {
        // PARITY 2.1.210: when the runner MOVES a timed-out foreground command to
        // the background (rather than killing it), the model sees the distinct
        // "…did not complete within its Ns timeout and was moved to the
        // background (ID: …)" note and the result carries `backgroundTaskId` +
        // `timedOutAfterMs` — NOT the interrupted/abort shape.
        struct MovedStub;
        #[async_trait]
        impl platform_api::process::ProcessRunner for MovedStub {
            async fn run(
                &self,
                _: &platform_api::sandbox::SandboxedCommand,
            ) -> Result<ProcessOutput, platform_api::process::ProcessError> {
                unreachable!("bash foreground uses run_foreground")
            }
            async fn run_foreground(
                &self,
                _: &platform_api::sandbox::SandboxedCommand,
            ) -> Result<platform_api::ForegroundOutcome, platform_api::process::ProcessError>
            {
                Ok(platform_api::ForegroundOutcome::MovedToBackground(
                    platform_api::process::ProcessHandle {
                        task_id: "local_bash_dead".into(),
                        pid: 4242,
                    },
                ))
            }
            async fn spawn_background(
                &self,
                _: &platform_api::sandbox::SandboxedCommand,
            ) -> Result<platform_api::process::ProcessHandle, platform_api::process::ProcessError>
            {
                unreachable!()
            }
            async fn kill(
                &self,
                _: &platform_api::process::ProcessHandle,
            ) -> Result<(), platform_api::process::ProcessError> {
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = std::sync::Arc::new(MovedStub);
        let tool = BashTool::new(ctx);
        let res = tool
            .call(
                json!({"command": "do-work", "timeout": 5000}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("moved-to-background is an Ok result");
        // Result data: empty output, not interrupted, carries the task id +
        // the exceeded timeout in ms (`timedOutAfterMs`).
        assert_eq!(res.data["interrupted"], false);
        assert_eq!(res.data["stdout"], "");
        assert_eq!(res.data["stderr"], "");
        assert_eq!(res.data["backgroundTaskId"], "local_bash_dead");
        assert_eq!(res.data["timedOutAfterMs"], 5000);
        assert!(res.data.get("timed_out").is_none());
        // Model note: the distinct timeout→background message (seconds =
        // round(5000/1000) = 5), NOT the abort marker.
        let mc = res.model_content.expect("model content present");
        assert!(
            mc.contains(
                "Command did not complete within its 5s timeout and was moved to the background (ID: local_bash_dead)."
            ),
            "expected timeout→background note, got: {mc}"
        );
        assert!(
            mc.contains("To check interim output, use Read on that file path."),
            "expected the shared background suffix, got: {mc}"
        );
        assert!(
            !mc.contains("Command was aborted before completion"),
            "must NOT surface the interrupted/abort marker: {mc}"
        );
    }

    // The sleep-gate tests used to have their own `SLEEP_GATE_LOCK`. They now
    // share `crate::prompt::background_env_lock()` with every other test in the
    // crate that touches these process-global gates — see that lock's doc for
    // why three separate locks over one global was no exclusion at all.

    fn bash_tool_noop() -> BashTool {
        BashTool::new(shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }))
    }

    #[tokio::test]
    async fn validate_input_blocks_standalone_sleep_when_gate_on() {
        let _g = crate::prompt::background_env_lock();
        let tool = bash_tool_noop();
        // Gate ON + duration >= 25 (G2n) → blocked with the byte-exact message.
        std::env::set_var("tengu_amber_sentinel", "1");
        let result = tool
            .validate_input(&json!({"command": "sleep 30"}), &use_ctx())
            .await;
        std::env::remove_var("tengu_amber_sentinel");
        let err = result.expect_err("sleep 30 must be blocked when the gate is on");
        assert_eq!(
            err.0,
            "Blocked: standalone sleep 30. To wait for a condition, use Monitor with an until-loop (e.g. `until <check>; do sleep 2; done`). To wait for a command you started, use run_in_background: true. Do not chain shorter sleeps to work around this block."
        );
    }

    #[tokio::test]
    async fn validate_input_blocks_sleep_with_followup_when_gate_on() {
        let _g = crate::prompt::background_env_lock();
        let tool = bash_tool_noop();
        std::env::set_var("tengu_amber_sentinel", "1");
        let result = tool
            .validate_input(&json!({"command": "sleep 30 && echo done"}), &use_ctx())
            .await;
        std::env::remove_var("tengu_amber_sentinel");
        let err = result.expect_err("sleep 30 && ... must be blocked when the gate is on");
        assert!(
            err.0
                .starts_with("Blocked: sleep 30 followed by: echo done. To wait"),
            "got: {}",
            err.0
        );
    }

    #[tokio::test]
    async fn validate_input_sleep_block_is_off_by_default() {
        let _g = crate::prompt::background_env_lock();
        let tool = bash_tool_noop();
        // No gate env set (default) → even a long sleep is allowed (1:1 with
        // stock claude-code, whose `sq()` defaults false).
        std::env::remove_var("tengu_amber_sentinel");
        tool.validate_input(&json!({"command": "sleep 30"}), &use_ctx())
            .await
            .expect("sleep 30 must be ALLOWED by default (gate off)");
    }

    #[tokio::test]
    async fn validate_input_allows_sub_threshold_and_float_even_when_gate_on() {
        let _g = crate::prompt::background_env_lock();
        let tool = bash_tool_noop();
        std::env::set_var("tengu_amber_sentinel", "1");
        // < 25 (incl. fractional, and non-sleep commands) are never blocked.
        let mut results = Vec::new();
        for ok in [
            "sleep 24",
            "sleep 24.9",
            "sleep 0.5",
            "echo hi",
            "sleeper foo",
        ] {
            results.push((
                ok,
                tool.validate_input(&json!({ "command": ok }), &use_ctx())
                    .await,
            ));
        }
        std::env::remove_var("tengu_amber_sentinel");
        for (ok, r) in results {
            r.unwrap_or_else(|e| panic!("{ok:?} should be allowed, got: {}", e.0));
        }
    }

    #[tokio::test]
    async fn validate_input_allows_sleep_when_backgrounded_even_with_gate_on() {
        let _g = crate::prompt::background_env_lock();
        let tool = bash_tool_noop();
        std::env::set_var("tengu_amber_sentinel", "1");
        let result = tool
            .validate_input(
                &json!({"command": "sleep 30", "run_in_background": true}),
                &use_ctx(),
            )
            .await;
        std::env::remove_var("tengu_amber_sentinel");
        result.expect("sleep 30 backgrounded must be allowed even with the gate on");
    }

    /// BASH-17 — the oracle's gate is `HSe() && !WA() && !e.run_in_background`.
    /// With background tasks disabled the block's own remedy
    /// (`run_in_background: true`) is not even in the schema, so the oracle
    /// stops blocking rather than dead-ending the model.
    #[tokio::test]
    async fn validate_input_does_not_block_sleep_when_background_tasks_are_disabled() {
        let _g = crate::prompt::background_env_lock();
        let tool = bash_tool_noop();
        std::env::set_var("tengu_amber_sentinel", "1");
        std::env::set_var("LINGXI_DISABLE_BACKGROUND_TASKS", "1");
        let disabled = tool
            .validate_input(&json!({"command": "sleep 30"}), &use_ctx())
            .await;
        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");
        // Same input, background tasks back on → still blocked, so the new
        // conjunct is the only thing that changed the verdict.
        let enabled = tool
            .validate_input(&json!({"command": "sleep 30"}), &use_ctx())
            .await;
        std::env::remove_var("tengu_amber_sentinel");
        disabled.expect("sleep 30 must be ALLOWED when background tasks are disabled");
        enabled.expect_err("sleep 30 must still be blocked when background tasks are enabled");
    }

    #[test]
    fn detect_blocked_sleep_pattern_threshold_and_fractional() {
        // Below the 25s threshold → not blocked.
        assert_eq!(detect_blocked_sleep_pattern("sleep 24"), None);
        assert_eq!(detect_blocked_sleep_pattern("sleep 24.9"), None);
        assert_eq!(detect_blocked_sleep_pattern("sleep 0.5"), None);
        // At/above threshold → blocked; integral renders WITHOUT a decimal.
        assert_eq!(
            detect_blocked_sleep_pattern("sleep 25"),
            Some("standalone sleep 25".to_string())
        );
        assert_eq!(
            detect_blocked_sleep_pattern("sleep 30"),
            Some("standalone sleep 30".to_string())
        );
        // Fractional duration is matched and kept (parseFloat semantics).
        assert_eq!(
            detect_blocked_sleep_pattern("sleep 30.5"),
            Some("standalone sleep 30.5".to_string())
        );
        // Trailing dot (`30.`) parses as 30 → "30".
        assert_eq!(
            detect_blocked_sleep_pattern("sleep 30."),
            Some("standalone sleep 30".to_string())
        );
        // Leading sleep with a follow-up command.
        assert_eq!(
            detect_blocked_sleep_pattern("sleep 40 && echo done"),
            Some("sleep 40 followed by: echo done".to_string())
        );
        // Non-matches.
        assert_eq!(detect_blocked_sleep_pattern("sleeper foo"), None);
        assert_eq!(detect_blocked_sleep_pattern("echo hi"), None);
    }

    #[test]
    fn timeout_env_resolvers_s0e_j3n() {
        // s0e(): unset / invalid / non-positive → 120000; positive → that value.
        assert_eq!(resolve_default_timeout_ms(None), 120_000);
        assert_eq!(resolve_default_timeout_ms(Some("")), 120_000);
        assert_eq!(resolve_default_timeout_ms(Some("abc")), 120_000);
        assert_eq!(resolve_default_timeout_ms(Some("0")), 120_000);
        assert_eq!(resolve_default_timeout_ms(Some("-5")), 120_000);
        assert_eq!(resolve_default_timeout_ms(Some("300000")), 300_000);
        assert_eq!(resolve_default_timeout_ms(Some("90000")), 90_000);
        // j3n(): unset → max(600000, default); positive → max(n, default).
        assert_eq!(resolve_max_timeout_ms(None, 120_000), 600_000);
        // A custom default above 600000 raises the floor (Math.max(_, s0e)).
        assert_eq!(resolve_max_timeout_ms(None, 900_000), 900_000);
        assert_eq!(resolve_max_timeout_ms(Some("1200000"), 120_000), 1_200_000);
        // A max BELOW the default is floored up to the default.
        assert_eq!(resolve_max_timeout_ms(Some("50000"), 120_000), 120_000);
        assert_eq!(resolve_max_timeout_ms(Some("bad"), 120_000), 600_000);
    }

    #[test]
    fn resolve_timeout_ms_vf_h5a_semantics() {
        // Plain number > 0 honored, even over the advisory 600000 "max" (#4).
        assert_eq!(resolve_timeout_ms(&json!({"timeout": 200})), 200);
        assert_eq!(resolve_timeout_ms(&json!({"timeout": 700_000})), 700_000);
        // Numeric STRING coerced (VF): "30000" → 30000 (#5).
        assert_eq!(resolve_timeout_ms(&json!({"timeout": "30000"})), 30_000);
        assert_eq!(resolve_timeout_ms(&json!({"timeout": " 5000 "})), 5_000);
        // 0 / negative / non-numeric string / absent → default (H5a).
        assert_eq!(
            resolve_timeout_ms(&json!({"timeout": 0})),
            BASH_DEFAULT_TIMEOUT_MS
        );
        assert_eq!(
            resolve_timeout_ms(&json!({"timeout": -5})),
            BASH_DEFAULT_TIMEOUT_MS
        );
        assert_eq!(
            resolve_timeout_ms(&json!({"timeout": "abc"})),
            BASH_DEFAULT_TIMEOUT_MS
        );
        assert_eq!(resolve_timeout_ms(&json!({})), BASH_DEFAULT_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn validate_input_no_longer_rejects_over_max_timeout() {
        // #4: an over-600000 timeout is accepted (no schema/runtime max).
        let tool = bash_tool_noop();
        tool.validate_input(
            &json!({"command": "echo hi", "timeout": 900_000}),
            &use_ctx(),
        )
        .await
        .expect("over-max timeout must be accepted (no max rejection)");
    }

    #[tokio::test]
    async fn foreground_strips_ansi_from_stdout() {
        let out = ProcessOutput {
            stdout: "\x1b[31mred\x1b[0m\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "printf-red"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        // ANSI stripped → "red\n", then normalized (trimEnd) → "red".
        assert_eq!(res.data["stdout"], "red");
    }

    #[tokio::test]
    async fn foreground_normalizes_leading_and_trailing_blank_lines() {
        let out = ProcessOutput {
            stdout: "\n\n\nhello\nworld\n\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "cat thing"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        // Leading blank lines stripped, trailing whitespace trimmed, inner kept.
        assert_eq!(res.data["stdout"], "hello\nworld");
    }

    #[tokio::test]
    async fn foreground_sets_no_output_expected_for_silent_command() {
        let out = ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "mkdir foo"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(res.data["noOutputExpected"], true);
    }

    #[tokio::test]
    async fn foreground_no_output_expected_false_for_non_silent_command() {
        let out = ProcessOutput {
            stdout: "a\nb\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "ls -la"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(res.data["noOutputExpected"], false);
    }

    // `BACKGROUND_TASKS_ENV_LOCK` lived here. `input_schema()` selects between
    // the full schema and the `run_in_background`-omitted one on
    // `LINGXI_DISABLE_BACKGROUND_TASKS` (claude-code 2.1.263 `gnr`/`Dl()`), and the
    // Bash PROMPT reads the same var for its detached-run bullet — so schema
    // tests and prompt tests must serialize against each other, not merely
    // within their own file. They all take
    // `crate::prompt::background_env_lock()` now.

    #[test]
    fn input_schema_uses_timeout_not_timeout_ms() {
        let _g = crate::prompt::background_env_lock();
        // claude-code `BashTool.tsx:229` names the param `timeout` (ms).
        let tool = BashTool::new(tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }));
        let props = &tool.input_schema()["properties"];
        assert!(
            props.get("timeout").is_some(),
            "schema must expose `timeout`"
        );
        assert!(
            props.get("timeout_ms").is_none(),
            "schema must NOT expose the old `timeout_ms`"
        );
        assert_eq!(
            props["timeout"]["description"],
            "Optional timeout in milliseconds (max 600000)"
        );
    }

    /// claude-code 2.1.238 `Qhm` (and `sdk-tools-238.d.ts`) declare the Bash input
    /// schema keys in this order; `serde_json` is built with `preserve_order`, so
    /// insertion order is what the model actually sees in the tool definition.
    #[test]
    fn input_schema_property_order_matches_oracle() {
        let _g = crate::prompt::background_env_lock();
        let tool = BashTool::new(tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }));
        let props = tool.input_schema()["properties"]
            .as_object()
            .expect("properties object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            props,
            vec![
                "command".to_string(),
                "timeout".to_string(),
                "description".to_string(),
                "run_in_background".to_string(),
                "dangerouslyDisableSandbox".to_string(),
            ]
        );
    }

    /// BASH-09 / claude-code 2.1.263 `gnr`:
    /// `Dl()?dnr().omit({run_in_background:!0,_simulatedSedEdit:!0}):…`.
    /// With background tasks disabled the property must vanish from the tool
    /// definition entirely — the prompt already drops its bullet on the same
    /// switch (`getBackgroundUsageNote`), so advertising the parameter would
    /// offer the model something nothing explains.
    #[test]
    fn input_schema_omits_run_in_background_when_background_tasks_are_disabled() {
        // ONE acquisition. This used to take two different locks — the schema
        // one and the sleep-gate one — because `LINGXI_DISABLE_BACKGROUND_TASKS`
        // is read by both families (BASH-17). Now that they are a single
        // crate-wide lock, taking it twice here would SELF-DEADLOCK:
        // `std::sync::Mutex` is not reentrant.
        let _g = crate::prompt::background_env_lock();
        let tool = BashTool::new(tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }));
        std::env::set_var("LINGXI_DISABLE_BACKGROUND_TASKS", "1");
        let keys = tool.input_schema()["properties"]
            .as_object()
            .expect("properties object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");
        // Every OTHER key keeps its oracle order and contents.
        assert_eq!(
            keys,
            vec![
                "command".to_string(),
                "timeout".to_string(),
                "description".to_string(),
                "dangerouslyDisableSandbox".to_string(),
            ]
        );
        // And it comes back when the gate is off.
        assert!(tool.input_schema()["properties"]
            .get("run_in_background")
            .is_some());
    }

    // ===== BASH-03 — `backgroundEndsWithFinalResponse` (2.1.238) =============

    /// `$2t` with `reapedAtFinalResponse` absent: byte-identical to the
    /// pre-2.1.238 note (both the explicit-background and the
    /// timeout-auto-background heads).
    #[test]
    fn background_note_default_keeps_the_notified_sentence() {
        assert_eq!(
            background_note("7", "/p", None, false),
            "Command running in background with ID: 7. Output is being written to: /p. You will be notified when it completes. To check interim output, use Read on that file path."
        );
        assert_eq!(
            background_note("7", "/p", Some(5000), false),
            "Command did not complete within its 5s timeout and was moved to the background (ID: 7). Output is being written to: /p. You will be notified when it completes. To check interim output, use Read on that file path."
        );
    }

    /// The 2.1.238 lifetime sentence, oracle binary @289989687. Note the U+2014
    /// EM DASH and the semicolon before "if you need its result".
    #[test]
    fn background_note_for_a_synchronous_subagent_warns_about_the_final_response() {
        assert_eq!(
            background_note("7", "/p", None, true),
            "Command running in background with ID: 7. Output is being written to: /p. If it exits while you are still working you will be notified, but it is terminated when you give your final response and no notification can follow that \u{2014} so do not end your turn to wait for it; if you need its result, wait for it before giving your final response. To check interim output, use Read on that file path."
        );
    }

    /// `wKo(agentContext)` = subagent AND NOT async.
    #[test]
    fn background_ends_with_final_response_only_for_a_synchronous_subagent() {
        let mut ctx = use_ctx();
        // Main loop (no agent id) — the command survives the turn.
        assert!(!background_ends_with_final_response(&ctx));
        ctx.agent_id = Some(protocol::AgentId::new());
        assert!(background_ends_with_final_response(&ctx));
        // Async / headless subagent: `is_non_interactive_session` is set by the
        // dispatch invoker from `is_async || effective_non_interactive_session()`.
        ctx.options.is_non_interactive_session = true;
        assert!(!background_ends_with_final_response(&ctx));
    }

    /// Output schema `At(!0).optional()` — the literal-`true` optional is
    /// emitted only when set, never as `false`, and sits after `timedOutAfterMs`.
    #[test]
    fn bash_result_data_emits_background_ends_with_final_response_only_when_true() {
        let reaped = bash_result_data(
            "",
            "",
            false,
            false,
            None,
            false,
            Some("t1"),
            None,
            Some(5000),
            true,
        );
        assert_eq!(reaped["backgroundEndsWithFinalResponse"], true);
        let keys = reaped
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let idx = |k: &str| keys.iter().position(|x| x == k).expect(k);
        assert!(idx("backgroundEndsWithFinalResponse") > idx("timedOutAfterMs"));
        let survives = bash_result_data(
            "",
            "",
            false,
            false,
            None,
            false,
            Some("t1"),
            None,
            None,
            false,
        );
        assert!(
            survives.get("backgroundEndsWithFinalResponse").is_none(),
            "`false` must be OMITTED, not serialised"
        );
    }

    // ===== BASH-06 — `staleReadFileStateHint` (`OcT` + `PcT`) ================

    #[test]
    fn write_command_markers_match_the_oracle_alternation() {
        for cmd in [
            "prettier --write .",
            "eslint --fix src",
            "sed --in-place s/a/b/ f",
            "rubocop --auto-correct",
            "npm run format",
            "npm run  fix",
            "yarn format",
            "pnpm format",
            "npm run lint:file",
            "npm run lint:fix",
            "black .",
            "isort .",
            "ruff format .",
            "cargo fmt",
            "cargo fix --allow-dirty",
            "rustfmt src/x.rs",
            "go fmt ./...",
            "terraform fmt",
            "dprint fmt",
            "swiftformat .",
            "phpcbf",
        ] {
            assert!(command_looks_like_a_writer(cmd), "should match: {cmd}");
        }
        for cmd in [
            "ls -la",
            "git status",
            "cargo build",
            "cargo   test",
            "blacklist-check",
            "myisort",
            "echo run",
            "rungo format",
            "go fmtx ./...",
        ] {
            assert!(!command_looks_like_a_writer(cmd), "must NOT match: {cmd}");
        }
    }

    /// Node `path.relative` for the two absolute paths the hint always has.
    #[test]
    fn path_relative_matches_node_semantics() {
        use std::path::Path;
        assert_eq!(
            path_relative(Path::new("/a/b"), Path::new("/a/b/c.rs")),
            "c.rs"
        );
        assert_eq!(path_relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(
            path_relative(Path::new("/a/b/c"), Path::new("/a/d/e.rs")),
            format!("..{s}..{s}d{s}e.rs", s = std::path::MAIN_SEPARATOR)
        );
    }

    fn seed_read_entry(ctx: &BuiltinToolContext, path: &std::path::Path, mtime_ms: i64) {
        tool_api::read_file_state::set(
            &ctx.read_file_state,
            path.to_path_buf(),
            tool_api::ReadFileEntry {
                content: "old".into(),
                mtime_ms,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
    }

    fn noop_ctx() -> BuiltinToolContext {
        shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }

    #[test]
    fn stale_read_file_state_hint_names_the_files_the_command_rewrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "new").expect("write");
        let ctx = noop_ctx();
        // The recorded read predates the on-disk mtime.
        seed_read_entry(&ctx, &file, 0);
        let hint = stale_read_file_state_hint(&ctx, "cargo fmt", dir.path(), 0)
            .expect("hint for a rewritten, previously-read file");
        assert_eq!(
            hint,
            "[This command modified 1 file you've previously read: a.rs. Call Read before editing.]"
        );
    }

    #[test]
    fn stale_read_file_state_hint_is_silent_without_a_write_marker_or_a_bump() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "new").expect("write");
        let ctx = noop_ctx();
        seed_read_entry(&ctx, &file, 0);
        // `PcT` does not match ⇒ the oracle returns [] before any stat.
        assert!(stale_read_file_state_hint(&ctx, "ls -la", dir.path(), 0).is_none());
        // Write marker, but the file's mtime predates the call start.
        assert!(stale_read_file_state_hint(&ctx, "cargo fmt", dir.path(), i64::MAX).is_none());
    }

    #[test]
    fn stale_read_file_state_hint_caps_the_list_at_five_and_pluralizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = noop_ctx();
        for i in 0..7 {
            let file = dir.path().join(format!("f{i}.rs"));
            std::fs::write(&file, "new").expect("write");
            seed_read_entry(&ctx, &file, 0);
        }
        let hint =
            stale_read_file_state_hint(&ctx, "prettier --write .", dir.path(), 0).expect("hint");
        // `keys()` is MRU-first, so the most recently seeded five are listed.
        assert_eq!(
            hint,
            "[This command modified 7 files you've previously read: f6.rs, f5.rs, f4.rs, f3.rs, f2.rs and 2 more. Call Read before editing.]"
        );
    }

    // ===== BASH-07 — `ghRateLimitHint` (oracle `ikf`/`V_v`/`K_v`) ===========

    /// `V_v` matches `gh` only as a COMMAND word, and only for a
    /// quota-spending subcommand.
    #[test]
    fn gh_invocation_predicate_matches_the_oracle_regex() {
        for yes in [
            "gh pr list",
            "gh api rate_limit",
            "git fetch && gh pr view 12",
            "cat x | gh pr create",
            "foo; gh issue list",
            "if x; then gh pr list; fi",
            "for f in *; do gh pr view; done",
            // No subcommand word at all — the lookahead cannot match `-h`.
            "gh -h",
            // `auth` only excludes the EXACT word (`auth\b`).
            "gh authorize",
        ] {
            assert!(
                command_invokes_rate_limited_gh(yes),
                "expected a gh invocation: {yes:?}"
            );
        }
        for no in [
            // `gh` is an argument, not a command word.
            "echo gh pr list",
            "grep gh file",
            // No whitespace after `gh` (`gh\s+` needs at least one).
            "gh",
            "ghost pr list",
            // `\b(?:then|do)\b` must be a whole word.
            "dogh pr list",
            // Excluded subcommands.
            "gh auth status",
            "gh help",
            "gh version",
            "gh alias list",
            "gh completion -s zsh",
            "gh config get editor",
        ] {
            assert!(
                !command_invokes_rate_limited_gh(no),
                "expected NO gh invocation: {no:?}"
            );
        }
    }

    /// Reproduced JS artifact: `gh\s+` is greedy WITH backtracking, so two or
    /// more separators let `\s+` end on a whitespace character where no excluded
    /// keyword can match — the negative lookahead then always succeeds.
    #[test]
    fn gh_invocation_predicate_reproduces_the_greedy_whitespace_backtrack() {
        assert!(!command_invokes_rate_limited_gh("gh auth status"));
        assert!(command_invokes_rate_limited_gh("gh  auth status"));
    }

    /// `K_v = /API rate limit (?:already )?exceeded|exceeded a secondary rate
    /// limit|\bRATE_LIMITED\b/i`.
    #[test]
    fn gh_rate_limit_output_predicate_matches_the_oracle_regex() {
        for yes in [
            "API rate limit exceeded for user ID 1.",
            "api rate limit already exceeded",
            "You have exceeded a secondary rate limit",
            "type: RATE_LIMITED",
            "rate_limited",
        ] {
            assert!(output_reports_gh_rate_limit(yes), "expected a hit: {yes:?}");
        }
        for no in [
            "",
            "API rate limit remaining: 4999",
            // `\b` before `RATE_LIMITED` fails — `_` is a word character.
            "X_RATE_LIMITED",
            "RATE_LIMITEDX",
        ] {
            assert!(!output_reports_gh_rate_limit(no), "expected a miss: {no:?}");
        }
    }

    /// `ikf` emits once, then backs off for `Y_v` = 60 000 ms.
    #[test]
    fn gh_rate_limit_hint_emits_once_then_backs_off_for_a_minute() {
        let key = "bash-07-backoff";
        let out = "API rate limit exceeded for user ID 1.";
        assert_eq!(
            gh_rate_limit_hint(key, "gh pr list", out, "", 1_000),
            Some(GH_RATE_LIMIT_REMINDER)
        );
        // Within the 60s window: suppressed.
        assert_eq!(gh_rate_limit_hint(key, "gh pr list", out, "", 60_999), None);
        // At the boundary (`Date.now() < backoffUntil` is false): emitted again.
        assert_eq!(
            gh_rate_limit_hint(key, "gh pr list", out, "", 61_000),
            Some(GH_RATE_LIMIT_REMINDER)
        );
    }

    /// Both predicates gate the hint, and the reminder text is byte-locked.
    #[test]
    fn gh_rate_limit_hint_requires_both_a_gh_command_and_a_rate_limit_output() {
        let out = "API rate limit exceeded";
        // Command matches but output does not.
        assert_eq!(
            gh_rate_limit_hint("bash-07-a", "gh pr list", "ok", "", 0),
            None
        );
        // Output matches but the command is not a gh invocation.
        assert_eq!(gh_rate_limit_hint("bash-07-b", "curl x", out, "", 0), None);
        // The oracle reads the FULL command output; the port checks both
        // streams because it does not merge stderr into stdout.
        assert_eq!(
            gh_rate_limit_hint("bash-07-c", "gh pr list", "", out, 0),
            Some(GH_RATE_LIMIT_REMINDER)
        );
        assert_eq!(
            GH_RATE_LIMIT_REMINDER,
            "<system-reminder>GitHub API rate limit exceeded (5,000/hr shared across all tools and agents). Run `gh api rate_limit --jq .resources` and sleep until reset before further gh calls. If polling in a loop, use ScheduleWakeup instead of retrying.</system-reminder>"
        );
    }

    /// Binary gap #1/#2: timeout type must be `number` (not `integer`), no `minimum`.
    /// Binary @203768858: `timeout:sB(A.number().optional())` — Zod `.number()` → `{"type":"number"}`.
    #[test]
    fn bash_timeout_schema_type_number_no_minimum() {
        let tool = BashTool::new(tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }));
        let timeout = &tool.input_schema()["properties"]["timeout"];
        assert_eq!(
            timeout["type"], "number",
            "timeout type must be number not integer"
        );
        assert!(
            timeout.get("minimum").is_none(),
            "timeout must have no minimum constraint"
        );
    }

    #[tokio::test]
    async fn foreground_success_sets_interrupted_false() {
        // claude-code `BashTool.tsx:283` outputSchema field `interrupted`: a
        // command that completes (does not time out) is not interrupted.
        let out = ProcessOutput {
            stdout: "done\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "echo done"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(res.data["interrupted"], false);
    }

    /// A1/STEP-4 — 2.1.220 does NOT truncate the model-facing Bash output.
    ///
    /// The shared shell truncator is GONE from the 2.1.220 binary: the only
    /// three `lines truncated] ...` sites left are prose in a prompt
    /// (@231867509), the diff/patch renderer (@232474811), and the NOTEBOOK
    /// output formatter `vtd` (@232559907, whose sole caller is `CCs`). The
    /// Bash tool's own `Jst()` use (@235703218) is just
    /// `D.length>Jst()` handed to the read-state nudge — `data.stdout` is the
    /// un-mutated `W`. `maxResultSizeChars:30000` (@235694594) is a
    /// PERSISTENCE threshold instead.
    ///
    /// Ground truth: across 3 271 real 2.1.220 transcripts the largest
    /// non-persisted Bash `tool_result` is 29 977 chars and 1 324 larger ones
    /// carry a `<persisted-output>` envelope. Zero carry a truncation suffix.
    #[tokio::test]
    async fn foreground_output_over_30k_is_passed_through_untruncated() {
        let head = "a".repeat(30_000);
        let tail = format!("{}tail", "\n".repeat(5));
        let out = ProcessOutput {
            stdout: format!("{head}{tail}"),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "yes"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        let s = res.data["stdout"].as_str().unwrap();
        assert!(
            !s.contains("lines truncated] ..."),
            "2.1.220 has no shell output truncator; got tail: {:?}",
            &s[s.len().saturating_sub(60)..]
        );
        assert_eq!(
            s,
            format!("{head}{}tail", "\n".repeat(5)),
            "stdout must reach the result mapper verbatim"
        );
        assert!(res.data.get("outputTaskId").is_none());
        assert!(res.data.get("outputFilePath").is_none());
        assert!(res.data.get("persistedOutputPath").is_none());
    }

    /// A completed task that was auto-backgrounded briefly retains its rooted
    /// output identity after the background id is cleared. The result map keeps
    /// the 2.1.263 persist fields together and in stable insertion order.
    #[tokio::test]
    async fn completed_auto_background_keeps_output_identity_without_background_id() {
        struct SpilledStub;
        #[async_trait]
        impl ProcessRunner for SpilledStub {
            async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
                unreachable!("Bash uses the foreground spill seam")
            }
            async fn run_foreground_with_output_limit(
                &self,
                _: &SandboxedCommand,
                _: Option<usize>,
            ) -> Result<ForegroundRunResult, ProcessError> {
                Ok(ForegroundRunResult {
                    outcome: platform_api::ForegroundOutcome::Completed(ProcessOutput {
                        stdout: "inline preview".into(),
                        stderr: String::new(),
                        exit_code: 0,
                        timed_out: false,
                    }),
                    // The runner has already rooted and pinned this path; the
                    // test models the auto-background completion transition by
                    // omitting any background handle while retaining it.
                    output_file: Some(ProcessOutputFile {
                        task_id: "local_bash_spilled".into(),
                        path: "/tmp/lingxi-task-output/local_bash_spilled.out".into(),
                        size: 30_001,
                    }),
                })
            }
            async fn spawn_background(
                &self,
                _: &SandboxedCommand,
            ) -> Result<ProcessHandle, ProcessError> {
                unreachable!()
            }
            async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }

        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = Arc::new(SpilledStub);
        let result = BashTool::new(ctx)
            .call(json!({"command": "printf output"}), use_ctx(), fresh_tx())
            .await
            .expect("spilled completion");

        assert!(result.data.get("backgroundTaskId").is_none());
        assert!(result.data.get("outputTaskId").is_none());
        assert!(result.data.get("outputFilePath").is_none());
        assert_eq!(
            result.data["persistedOutputPath"],
            "/tmp/lingxi-task-output/local_bash_spilled.out"
        );
        assert_eq!(result.data["persistedOutputSize"], 30_001);
        let keys = result
            .data
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "stdout",
                "stderr",
                "interrupted",
                "isImage",
                "noOutputExpected",
                "persistedOutputPath",
                "persistedOutputSize",
            ]
        );
    }

    /// The threshold the orchestrator's persistence layer reads for Bash —
    /// `maxResultSizeChars:30000` (2.1.220 BIN off **235694594**) folded
    /// through `M0u` as `min(30000, AKr=50000)`.
    #[test]
    fn bash_declares_a_30k_persistence_threshold() {
        use tool_api::tool_trait::Tool as _;
        let out = ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        assert_eq!(tool.persistence_threshold(), Some(30_000));
    }

    // ----- Background path: bespoke stub that returns a fake ProcessHandle. -----

    use platform_api::process::{
        ForegroundRunResult, ProcessError, ProcessHandle, ProcessOutputFile, ProcessRunner,
    };
    use platform_api::sandbox::SandboxedCommand;
    use std::sync::Arc;

    struct BgStub;
    #[async_trait]
    impl ProcessRunner for BgStub {
        async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            unreachable!()
        }
        async fn spawn_background(
            &self,
            _: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Ok(ProcessHandle {
                task_id: "task-abc123".into(),
                pid: 4242,
            })
        }
        async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// bg-08 wiring: the oracle's own backgrounding event has to actually FIRE
    /// on the explicit-background path, carrying the `command_type`. Pinning the
    /// classifier alone would leave the emitter free to not exist.
    #[tokio::test]
    async fn an_explicit_background_launch_emits_the_oracle_event() {
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = Arc::new(BgStub);
        let sink = Arc::new(telemetry::InMemorySink::default());
        ctx.bus.attach_sink(sink.clone()).await;
        BashTool::new(ctx)
            .call(
                json!({"command": "cd repo && cargo build", "run_in_background": true}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");

        let events = sink.events().await;
        let hit = events
            .iter()
            .find(|e| e.name == "tengu_bash_command_explicitly_backgrounded")
            .expect("the explicit-background event must fire");
        let Some(telemetry::AnalyticsValue::String(command_type)) =
            hit.metadata.get("command_type")
        else {
            panic!(
                "the event must carry a string `command_type`: {:?}",
                hit.metadata
            );
        };
        assert_eq!(
            command_type, "cargo",
            "the first KNOWN command name across the line, not `cd`"
        );
    }

    #[tokio::test]
    async fn background_returns_background_task_id() {
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = Arc::new(BgStub);
        let tool = BashTool::new(ctx);
        let res = tool
            .call(
                json!({"command": "sleep 5", "run_in_background": true}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        // claude-code backgrounded result: the main shape with `backgroundTaskId`
        // + empty stdout/stderr. No `pid`/`task_output_path` in `data` — the ID
        // and output path ride in the model note (`model_content`).
        assert_eq!(res.data["backgroundTaskId"], "task-abc123");
        assert_eq!(res.data["stdout"], "");
        assert_eq!(res.data["interrupted"], false);
        assert!(res.data.get("pid").is_none());
        assert!(res.data.get("task_output_path").is_none());
        let note = res.model_content.as_deref().expect("background model note");
        assert!(
            note.contains("task-abc123") && note.contains("Command running in background"),
            "note must carry the task id + path, got: {note}",
        );
        // A `cd`-free command gets NO `backgroundCwdHint` (`ror` is false).
        assert!(
            !note.contains("Session cwd remains"),
            "non-cd bg command must not carry the cwd hint, got: {note}",
        );
    }

    /// Records what the Bash tool asks of the task registry, so a test can
    /// assert the background path mints ONE identity and registers it.
    #[derive(Default)]
    struct RecordingRegistry {
        fail_register: bool,
        fail_bind: bool,
        bound_pids: std::sync::Mutex<Vec<u32>>,
        output_chunks: std::sync::Mutex<Vec<(String, String)>>,
        output_flushes: std::sync::Mutex<Vec<String>>,

        allocated: std::sync::Mutex<Vec<String>>,
        registered: std::sync::Mutex<
            Vec<(
                String,
                platform_api::task_registry::BackgroundBashRegistration,
            )>,
        >,
        bound: std::sync::Mutex<Vec<String>>,
        discarded: std::sync::Mutex<Vec<String>>,
        armed: std::sync::Mutex<Vec<String>>,
        requesters: std::sync::Mutex<Vec<String>>,
        unarmed: std::sync::Mutex<Vec<String>>,
    }

    #[tokio::test]
    async fn background_bash_sink_routes_output_and_flush_to_registry_writer() {
        use platform_api::BackgroundExitSink as _;
        let registry = std::sync::Arc::new(RecordingRegistry::default());
        let sink = BackgroundBashExitSink {
            registry: registry.clone(),
        };
        assert!(sink.manages_output());
        sink.append_output("boutput01", "[stderr] 中🦀")
            .await
            .unwrap();
        sink.flush_output("boutput01").await.unwrap();
        assert_eq!(
            registry.output_chunks.lock().unwrap().as_slice(),
            [("boutput01".to_string(), "[stderr] 中🦀".to_string())]
        );
        assert_eq!(
            registry.output_flushes.lock().unwrap().as_slice(),
            ["boutput01"]
        );
    }

    #[async_trait::async_trait]
    impl platform_api::task_registry::TaskRegistryHandle for RecordingRegistry {
        async fn append_bash_output(
            &self,
            task_id: &str,
            content: &str,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            self.output_chunks
                .lock()
                .unwrap()
                .push((task_id.into(), content.into()));
            Ok(())
        }
        async fn flush_bash_output(
            &self,
            task_id: &str,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            self.output_flushes.lock().unwrap().push(task_id.into());
            Ok(())
        }

        async fn register_foreground_bash(
            &self,
            task_id: &str,
            _r: platform_api::task_registry::BackgroundBashRegistration,
            _armed: bool,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            self.armed.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
        async fn unregister_foreground_bash(&self, task_id: &str) {
            self.unarmed.lock().unwrap().push(task_id.to_string());
        }
        async fn bind_background_requester(
            &self,
            task_id: &str,
            _r: std::sync::Arc<dyn platform_api::task_registry::TaskBackgrounder>,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            self.requesters.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
        async fn create(
            &self,
            _i: platform_api::task_registry::TaskCreateInput,
        ) -> Result<
            platform_api::task_registry::TaskRecord,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn get(
            &self,
            _id: &str,
        ) -> Result<
            Option<platform_api::task_registry::TaskRecord>,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn list(
            &self,
            _f: platform_api::task_registry::TaskListFilter,
        ) -> Result<
            Vec<platform_api::task_registry::TaskRecord>,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn update(
            &self,
            _id: &str,
            _p: platform_api::task_registry::TaskUpdatePatch,
        ) -> Result<
            platform_api::task_registry::TaskRecord,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn set_status(
            &self,
            _id: &str,
            _s: &str,
        ) -> Result<
            platform_api::task_registry::TaskRecord,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn kill(
            &self,
            _id: &str,
        ) -> Result<
            platform_api::task_registry::TaskRecord,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn output(
            &self,
            _id: &str,
            _o: Option<u64>,
        ) -> Result<
            platform_api::task_registry::TaskOutputChunk,
            platform_api::task_registry::TaskRegistryError,
        > {
            unreachable!()
        }
        async fn allocate_bash_output(
            &self,
        ) -> Result<
            platform_api::task_registry::BackgroundBashHandle,
            platform_api::task_registry::TaskRegistryError,
        > {
            let id = "b1a2b3c4d".to_string();
            self.allocated.lock().unwrap().push(id.clone());
            Ok(platform_api::task_registry::BackgroundBashHandle {
                task_id: id,
                output_path: "/session/tasks/b1a2b3c4d.output".to_string(),
            })
        }
        async fn register_background_bash(
            &self,
            task_id: &str,
            registration: platform_api::task_registry::BackgroundBashRegistration,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            if self.fail_register {
                return Err(platform_api::task_registry::TaskRegistryError::Internal(
                    "registration failed".into(),
                ));
            }
            self.registered
                .lock()
                .unwrap()
                .push((task_id.to_string(), registration));
            Ok(())
        }
        async fn discard_bash_output(&self, task_id: &str) {
            self.discarded.lock().unwrap().push(task_id.to_string());
        }
        async fn bind_background_process(
            &self,
            id: &str,
            pid: u32,
            killer: std::sync::Arc<dyn platform_api::task_registry::TaskKiller>,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            if self.fail_bind {
                return Err(platform_api::task_registry::TaskRegistryError::Internal(
                    "binding failed".into(),
                ));
            }
            self.bound_pids.lock().unwrap().push(pid);
            self.bind_background_killer(id, killer).await
        }

        async fn bind_background_killer(
            &self,
            id: &str,
            _killer: std::sync::Arc<dyn platform_api::task_registry::TaskKiller>,
        ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
            self.bound.lock().unwrap().push(id.to_string());
            Ok(())
        }
    }

    /// A runner that honours a bound task identity, as the real posix runner
    /// does: it reports back the id the caller bound rather than minting one.
    struct BindingAwareBgStub;

    #[async_trait::async_trait]
    impl ProcessRunner for BindingAwareBgStub {
        async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            unreachable!("background test never runs in the foreground")
        }
        async fn spawn_background(
            &self,
            cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            let task_id = cmd
                .background_task()
                .map_or_else(|| "runner-minted".to_string(), |b| b.task_id.clone());
            Ok(ProcessHandle { task_id, pid: 4242 })
        }
        async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn foreground_disarm_joins_timer_before_withdrawing_its_row() {
        struct TimerDropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for TimerDropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ready = Arc::new(tokio::sync::Notify::new());
        let timer = tokio::spawn({
            let dropped = dropped.clone();
            let ready = ready.clone();
            async move {
                let _guard = TimerDropped(dropped);
                ready.notify_one();
                std::future::pending::<()>().await;
            }
        });
        ready.notified().await;
        let registry = Arc::new(RecordingRegistry::default());
        ForegroundArming {
            timer: Some(timer),
            withdraw_on_drop: true,
            task_id: "barming01".into(),
            registry: registry.clone(),
        }
        .disarm()
        .await;
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(*registry.unarmed.lock().unwrap(), vec!["barming01"]);
    }

    #[tokio::test]
    async fn dropping_bash_call_cancels_arming_and_withdraws_only_foreground_state() {
        struct PendingRunner {
            entered: Arc<tokio::sync::Notify>,
            kills: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ProcessRunner for PendingRunner {
            fn supports_foreground_backgrounding(&self) -> bool {
                true
            }
            async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
                unreachable!()
            }
            async fn run_foreground_with_output_limit(
                &self,
                _: &SandboxedCommand,
                _: Option<usize>,
            ) -> Result<platform_api::ForegroundRunResult, ProcessError> {
                self.entered.notify_one();
                std::future::pending().await
            }
            async fn spawn_background(
                &self,
                _: &SandboxedCommand,
            ) -> Result<ProcessHandle, ProcessError> {
                unreachable!()
            }
            async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
                self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        for arm_first in [false, true] {
            let registry = Arc::new(RecordingRegistry::default());
            let entered = Arc::new(tokio::sync::Notify::new());
            let runner = Arc::new(PendingRunner {
                entered: entered.clone(),
                kills: Default::default(),
            });
            let mut ctx = shell_test_ctx(ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            });
            ctx.task_registry = Some(registry.clone());
            ctx.process = runner.clone();
            let tool = BashTool::new(ctx);
            let mut call =
                Box::pin(tool.call(json!({"command":"sleep 10"}), use_ctx(), fresh_tx()));
            tokio::select! {
                result = &mut call => panic!("pending runner unexpectedly returned {result:?}"),
                () = entered.notified() => {},
            }
            if arm_first {
                tokio::time::sleep(FOREGROUND_ARMING_DELAY + std::time::Duration::from_millis(100))
                    .await;
                assert_eq!(registry.armed.lock().unwrap().len(), 1);
            }
            // Models the owning host aborting its task without polling Bash's
            // cooperative cancellation branch again.
            drop(call);
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if !registry.unarmed.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("dropping the call must withdraw its foreground row");
            if !arm_first {
                tokio::time::sleep(FOREGROUND_ARMING_DELAY + std::time::Duration::from_millis(100))
                    .await;
                assert!(
                    registry.armed.lock().unwrap().is_empty(),
                    "detached arming timer published a phantom task after call drop"
                );
            }
            assert_eq!(
                runner.kills.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "arming cleanup does not kill background processes"
            );
        }
    }

    #[tokio::test]
    async fn promoted_arming_does_not_withdraw_background_ownership_on_drop() {
        let registry = Arc::new(RecordingRegistry::default());
        let timer = tokio::spawn(std::future::pending::<()>());
        ForegroundArming {
            timer: Some(timer),
            withdraw_on_drop: true,
            task_id: "bpromoted".into(),
            registry: registry.clone(),
        }
        .keep_for_background()
        .await;
        tokio::task::yield_now().await;
        assert!(registry.unarmed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn background_registration_and_pid_binding_precede_detachment_ack() {
        struct AckRunner {
            registry: Arc<RecordingRegistry>,
            acks: std::sync::atomic::AtomicUsize,
            kills: std::sync::atomic::AtomicUsize,
            fail_ack: bool,
        }
        #[async_trait::async_trait]
        impl ProcessRunner for AckRunner {
            async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
                unreachable!()
            }
            async fn spawn_background(
                &self,
                cmd: &SandboxedCommand,
            ) -> Result<ProcessHandle, ProcessError> {
                BindingAwareBgStub.spawn_background(cmd).await
            }
            async fn acknowledge_shell(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
                assert_eq!(
                    self.registry.registered.lock().unwrap()[0].0,
                    handle.task_id
                );
                assert_eq!(*self.registry.bound_pids.lock().unwrap(), vec![handle.pid]);
                self.acks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.fail_ack {
                    Err(ProcessError::Io("ack failed".into()))
                } else {
                    Ok(())
                }
            }
            async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
                self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        for failure in ["none", "register", "bind", "ack"] {
            let registry = Arc::new(RecordingRegistry {
                fail_register: failure == "register",
                fail_bind: failure == "bind",
                ..Default::default()
            });
            let runner = Arc::new(AckRunner {
                registry: registry.clone(),
                acks: Default::default(),
                kills: Default::default(),
                fail_ack: failure == "ack",
            });
            let mut ctx = shell_test_ctx(ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            });
            ctx.process = runner.clone();
            ctx.task_registry = Some(registry);
            let result = BashTool::new(ctx)
                .call(
                    json!({"command":"sleep 5", "run_in_background":true}),
                    use_ctx(),
                    fresh_tx(),
                )
                .await;
            assert_eq!(result.is_ok(), failure == "none", "{failure}");
            assert_eq!(
                runner.acks.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(matches!(failure, "none" | "ack")),
                "{failure}"
            );
            assert_eq!(
                runner.kills.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(failure != "none"),
                "{failure}"
            );
        }
    }

    #[tokio::test]
    async fn background_task_id_is_the_registry_id_and_the_task_is_registered() {
        // The defect this pins: the model was handed the runner's private id
        // (`local_bash_<pid>_<nonce>_<seq>`) while the registry knew nothing, so
        // TaskOutput/TaskStop on that id answered "No task found with ID".
        // claude-code mints ONE identity per shell command (`vV`) and registers
        // that same id when the command is backgrounded (`Xne`).
        let registry = Arc::new(RecordingRegistry::default());
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = Arc::new(BindingAwareBgStub);
        ctx.task_registry = Some(registry.clone());
        let tool = BashTool::new(ctx);
        let res = tool
            .call(
                json!({"command": "sleep 5", "description": "wait a bit", "run_in_background": true}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");

        // The id the model sees is the registry's, NOT the runner stub's.
        assert_eq!(res.data["backgroundTaskId"], "b1a2b3c4d");
        assert_eq!(registry.allocated.lock().unwrap().len(), 1);

        // …and a record exists for exactly that id, carrying the command and
        // the description the completion summary renders.
        let registered = registry.registered.lock().unwrap().clone();
        assert_eq!(registered.len(), 1, "exactly one record for one command");
        assert_eq!(registered[0].0, "b1a2b3c4d");
        assert_eq!(registered[0].1.command, "sleep 5");
        assert_eq!(registered[0].1.description, "wait a bit");

        // The killer is bound so TaskStop can reach the child.
        assert_eq!(registry.bound.lock().unwrap().as_slice(), ["b1a2b3c4d"]);
        // Nothing was discarded: the identity was used, not redundant.
        assert!(registry.discarded.lock().unwrap().is_empty());

        // The note points at the registry's output file, which is the file the
        // registry can actually read back through TaskOutput.
        let note = res.model_content.as_deref().expect("background model note");
        assert!(
            note.contains("b1a2b3c4d") && note.contains("/session/tasks/b1a2b3c4d.output"),
            "note must carry the registry id and its output path, got: {note}",
        );
    }

    #[tokio::test]
    async fn foreground_command_registers_no_task_and_discards_its_output_file() {
        // claude-code mints the identity for every shell command but only
        // creates a record when the command is actually backgrounded. Creating
        // one per foreground command would put a completed row in TaskList and a
        // `<task-notification>` in the transcript for every shell call.
        let registry = Arc::new(RecordingRegistry::default());
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: "done\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.task_registry = Some(registry.clone());
        let tool = BashTool::new(ctx);
        let res = tool
            .call(json!({"command": "echo done"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");

        assert!(res.data.get("backgroundTaskId").is_none());
        assert!(
            registry.registered.lock().unwrap().is_empty(),
            "a foreground command must not become a task record",
        );
        assert_eq!(
            registry.discarded.lock().unwrap().as_slice(),
            ["b1a2b3c4d"],
            "the unused output file must be discarded, not left behind",
        );
    }

    /// claude-code `U6t` after `cnr` = 2000 ms, then `W6t` on foreground
    /// completion. This is the wiring test: the pieces are unit-tested
    /// elsewhere, but nothing else proves `call()` actually starts the timer,
    /// binds the requester, and withdraws the row afterwards.
    #[tokio::test]
    async fn a_slow_foreground_command_arms_a_row_and_gives_it_back() {
        let _gate = crate::prompt::background_env_lock();
        struct SlowRunner;
        #[async_trait::async_trait]
        impl ProcessRunner for SlowRunner {
            fn supports_foreground_backgrounding(&self) -> bool {
                true
            }
            async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
                unreachable!()
            }
            async fn run_foreground_with_output_limit(
                &self,
                _cmd: &SandboxedCommand,
                _max: Option<usize>,
            ) -> Result<platform_api::ForegroundRunResult, ProcessError> {
                // Past `cnr`, then a normal foreground finish.
                tokio::time::sleep(std::time::Duration::from_millis(2400)).await;
                Ok(platform_api::ForegroundRunResult {
                    outcome: platform_api::ForegroundOutcome::Completed(ProcessOutput {
                        stdout: "done\n".into(),
                        stderr: String::new(),
                        exit_code: 0,
                        timed_out: false,
                    }),
                    output_file: None,
                })
            }
            async fn spawn_background(
                &self,
                _: &SandboxedCommand,
            ) -> Result<ProcessHandle, ProcessError> {
                unreachable!()
            }
            async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }

        let registry = Arc::new(RecordingRegistry::default());
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.task_registry = Some(registry.clone());
        ctx.process = Arc::new(SlowRunner);
        let tool = BashTool::new(ctx);
        let (progress, mut progress_rx) = tool_api::progress::progress_channel();
        let mut use_context = use_ctx();
        use_context.tool_use_id = Some(protocol::ToolUseId::new());
        tool.call(json!({"command": "sleep 3"}), use_context, progress)
            .await
            .expect("ok");

        let hint = progress_rx
            .try_recv()
            .expect("foreground arming must publish background_hint");
        assert_eq!(hint.data["kind"], "background_hint");
        assert_eq!(hint.data["toolUseId"], hint.tool_use_id.to_string());
        assert_eq!(
            registry.armed.lock().unwrap().as_slice(),
            ["b1a2b3c4d"],
            "a command still running at 2 s must become an addressable row",
        );
        assert_eq!(
            registry.requesters.lock().unwrap().as_slice(),
            ["b1a2b3c4d"],
            "and it must carry the handle that backgrounds it, or Ctrl+B has no target",
        );
        assert_eq!(
            registry.unarmed.lock().unwrap().as_slice(),
            ["b1a2b3c4d"],
            "finishing in the foreground must give the row back (`W6t`)",
        );
        assert!(
            registry.registered.lock().unwrap().is_empty(),
            "arming is not a background registration",
        );
    }

    #[test]
    fn first_statement_is_sleep_matches_the_oracle_predicate() {
        // `$es`: only the FIRST word of the FIRST statement counts.
        assert!(first_statement_is_sleep("sleep 30"));
        assert!(first_statement_is_sleep("  sleep 5 && echo done"));
        assert!(first_statement_is_sleep("sleep 0.5"));
        // A sleep that is not the leading statement does not disqualify.
        assert!(!first_statement_is_sleep("echo hi && sleep 30"));
        assert!(!first_statement_is_sleep("cargo build"));
        // Unlike the sleep-BLOCK predicate there is no duration threshold and no
        // standalone requirement, so a short or trailing-argument sleep counts.
        assert!(first_statement_is_sleep("sleep 1"));
        // Both of `$es`'s early returns: nothing to inspect means eligible.
        assert!(!first_statement_is_sleep(""));
        assert!(!first_statement_is_sleep("   "));
    }

    #[tokio::test]
    async fn a_sleep_command_is_not_eligible_for_auto_background() {
        // claude-code computes `dn = !Dl() && $es(command)` and hands it to the
        // shell as `shouldAutoBackground`; with it false the deadline kills the
        // child instead of backgrounding it.
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let recorded = Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));

        struct FlagRecorder {
            seen: Arc<std::sync::Mutex<Vec<bool>>>,
        }
        #[async_trait::async_trait]
        impl ProcessRunner for FlagRecorder {
            async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
                unreachable!()
            }
            async fn run_foreground_with_output_limit(
                &self,
                cmd: &SandboxedCommand,
                _max: Option<usize>,
            ) -> Result<platform_api::ForegroundRunResult, ProcessError> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(cmd.auto_background_on_timeout());
                Ok(platform_api::ForegroundRunResult {
                    outcome: platform_api::ForegroundOutcome::Completed(ProcessOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                        timed_out: false,
                    }),
                    output_file: None,
                })
            }
            async fn spawn_background(
                &self,
                _: &SandboxedCommand,
            ) -> Result<ProcessHandle, ProcessError> {
                unreachable!()
            }
            async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }

        let mut sleep_ctx = ctx;
        sleep_ctx.process = Arc::new(FlagRecorder {
            seen: recorded.clone(),
        });
        let tool = BashTool::new(sleep_ctx.clone());
        tool.call(json!({"command": "sleep 30"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        tool.call(json!({"command": "cargo build"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");

        assert_eq!(
            recorded.lock().unwrap().as_slice(),
            [false, true],
            "a sleep-led command must not be auto-backgroundable; an ordinary one must be",
        );
    }

    #[tokio::test]
    async fn background_cd_command_appends_session_cwd_hint() {
        // PARITY 2.1.210 (`backgroundCwdHint`): a backgrounded command containing
        // a statement-level `cd` gets the "Session cwd remains …" hint appended on
        // a new line so the model does not assume the `cd` took effect.
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.process = Arc::new(BgStub);
        let tool = BashTool::new(ctx);
        let res = tool
            .call(
                json!({"command": "cd /tmp && sleep 5", "run_in_background": true}),
                use_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let note = res.model_content.as_deref().expect("background model note");
        assert!(
            note.contains("Command running in background"),
            "base bg note missing, got: {note}",
        );
        assert!(
            note.contains(
                "; directory changes made by the backgrounded command do not apply to subsequent commands."
            ),
            "cd bg command must carry the session-cwd hint, got: {note}",
        );
        // The hint rides on its own line after the base note (mapper `_+="\n"+a`).
        assert!(
            note.contains("\nSession cwd remains "),
            "hint must be appended on a new line, got: {note}",
        );
    }

    // ----- BASH.1 / BASH.2 / BASH.3 / BASH.5 ports -------------------------

    #[test]
    fn disable_extglob_command_byte_locked_per_shell() {
        let _g = shell_env_lock();
        // The LINGXI_SHELL_PREFIX branch overrides the shell-specific form;
        // only assert the per-shell strings when that env var is unset.
        if !std::env::var("LINGXI_SHELL_PREFIX").is_ok_and(|v| !v.is_empty()) {
            assert_eq!(
                disable_extglob_command("/bin/bash").as_deref(),
                Some("shopt -u extglob 2>/dev/null || true")
            );
            assert_eq!(
                disable_extglob_command("/bin/zsh").as_deref(),
                Some("setopt NO_EXTENDED_GLOB 2>/dev/null || true")
            );
            assert_eq!(disable_extglob_command("/usr/bin/fish"), None);
        }
    }

    #[test]
    fn null_redirect_rewrite_matches_ts_regex() {
        // Rewritten cases (case-insensitive `nul`, optional fd/`&`/whitespace).
        assert_eq!(rewrite_windows_null_redirect("ls 2>nul"), "ls 2>/dev/null");
        assert_eq!(rewrite_windows_null_redirect("ls >nul"), "ls >/dev/null");
        assert_eq!(rewrite_windows_null_redirect("ls > NUL"), "ls > /dev/null");
        assert_eq!(rewrite_windows_null_redirect("ls &>nul"), "ls &>/dev/null");
        assert_eq!(rewrite_windows_null_redirect("ls >>nul"), "ls >>/dev/null");
        assert_eq!(
            rewrite_windows_null_redirect("a 2>nul | b"),
            "a 2>/dev/null | b"
        );
        assert_eq!(
            rewrite_windows_null_redirect("(x 2>nul)"),
            "(x 2>/dev/null)"
        );
        // Non-matching cases (must pass through unchanged).
        assert_eq!(rewrite_windows_null_redirect("ls >null"), "ls >null");
        assert_eq!(
            rewrite_windows_null_redirect("ls >nullable"),
            "ls >nullable"
        );
        assert_eq!(rewrite_windows_null_redirect("ls >nul.txt"), "ls >nul.txt");
        assert_eq!(rewrite_windows_null_redirect("cat nul.txt"), "cat nul.txt");
        // UTF-8 passthrough around a rewritten redirect.
        assert_eq!(
            rewrite_windows_null_redirect("echo café 2>nul"),
            "echo café 2>/dev/null"
        );
    }

    #[test]
    fn resolve_max_output_length_honors_env_value() {
        assert_eq!(resolve_max_output_length(None), 30_000); // unset → default
        assert_eq!(resolve_max_output_length(Some("")), 30_000); // empty → default
        assert_eq!(resolve_max_output_length(Some("100")), 100); // valid
        assert_eq!(resolve_max_output_length(Some("123abc")), 123); // parseInt prefix
        assert_eq!(resolve_max_output_length(Some("abc")), 30_000); // NaN → default
        assert_eq!(resolve_max_output_length(Some("0")), 30_000); // 0 → default
        assert_eq!(resolve_max_output_length(Some("-5")), 30_000); // negative → default
        assert_eq!(resolve_max_output_length(Some("999999")), 150_000); // capped
        assert_eq!(resolve_max_output_length(Some("150000")), 150_000); // at limit
                                                                        // The env-reading wrapper falls back to the default when unset.
        if std::env::var_os("BASH_MAX_OUTPUT_LENGTH").is_none() {
            assert_eq!(bash_max_output_length(), 30_000);
        }
    }

    /// TL-3 (2.1.261) — `settings.bashOutputMaxChars` is read through the same
    /// `see()` clamp as `taskOutputMaxChars` and WINS over the env var. Before
    /// this the Bash cap came from `BASH_MAX_OUTPUT_LENGTH` alone, so a project
    /// that set the setting saw no effect.
    ///
    /// Serialized with the other process-global publishers: this asserts on a
    /// process-wide cell, so it must not race a parallel test that sets it.
    #[test]
    fn the_bash_output_setting_is_clamped_and_beats_the_env_var() {
        let _guard = bash_output_setting_lock();
        // Unset ⇒ the env/default path is untouched.
        platform_api::session_flags::set_bash_output_max_chars(None);
        if std::env::var_os("BASH_MAX_OUTPUT_LENGTH").is_none() {
            assert_eq!(bash_max_output_length(), BASH_MAX_OUTPUT_DEFAULT);
        }

        // In range: used verbatim, and it beats the env default.
        platform_api::session_flags::set_bash_output_max_chars(Some(50_000));
        assert_eq!(bash_max_output_length(), 50_000);

        // Out of range: PULLED to the nearest bound, never rejected.
        platform_api::session_flags::set_bash_output_max_chars(Some(1));
        assert_eq!(bash_max_output_length(), BASH_OUTPUT_SETTING_MIN);
        platform_api::session_flags::set_bash_output_max_chars(Some(10_000_000));
        assert_eq!(bash_max_output_length(), BASH_OUTPUT_SETTING_MAX);

        // The CHANGELOG's "up to 128K characters".
        assert_eq!(BASH_OUTPUT_SETTING_MAX, 128_000);

        platform_api::session_flags::set_bash_output_max_chars(None);
    }

    /// The publisher is a process global, so the test above must not run
    /// concurrently with anything else that writes it.
    fn bash_output_setting_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // Capturing runner that records the argv handed to the process runner so we
    // can assert what actually gets spawned.
    struct CapturingRunner {
        out: ProcessOutput,
        last_args: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl ProcessRunner for CapturingRunner {
        async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.last_args.lock().unwrap().clone_from(&cmd.inner().args);
            Ok(self.out.clone())
        }
        async fn spawn_background(
            &self,
            _: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            unreachable!()
        }
        async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    fn ok_output() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    #[tokio::test]
    async fn foreground_spawn_prepends_extglob_disable_prefix() {
        let last = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut ctx = shell_test_ctx(ok_output());
        ctx.process = Arc::new(CapturingRunner {
            out: ok_output(),
            last_args: last.clone(),
        });
        let tool = BashTool::new(ctx);
        tool.call(json!({"command": "echo hi"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        let args = last.lock().unwrap().clone();
        // Spawn shape: `-c -l <command>`.
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "-l");
        let spawned = &args[2];
        let expected_prefix = disable_extglob_command(resolve_shell_path())
            .expect("known host shell has an extglob-disable prefix");
        assert!(
            spawned.starts_with(&format!("{expected_prefix} && ")),
            "extglob disable must lead the spawned command, got: {spawned}"
        );
        // The user command and the BASH.4 cwd readback survive after the prefix.
        assert!(spawned.contains("echo hi"), "got: {spawned}");
        assert!(spawned.contains("pwd -P >|"), "got: {spawned}");
    }

    #[tokio::test]
    async fn dangerously_disable_sandbox_bypasses_when_unsandboxed_allowed() {
        let last = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut ctx = shell_test_ctx(ok_output());
        // Force the default decision to Sandbox: available sandbox + no excluded
        // commands yields `Sandbox { .. }`.
        ctx.sandbox_available = true;
        ctx.sandbox_runtime.excluded_commands = vec![];
        ctx.sandbox_runtime.allow_unsandboxed_commands = true;
        ctx.process = Arc::new(CapturingRunner {
            out: ok_output(),
            last_args: last.clone(),
        });
        let tool = BashTool::new(ctx);

        // Baseline (no flag): the command is sandbox-wrapped.
        tool.call(json!({"command": "echo hi"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        let wrapped = last.lock().unwrap()[2].clone();
        assert!(
            wrapped.contains("sandbox-exec") || wrapped.contains("bwrap"),
            "baseline should be sandbox-wrapped, got: {wrapped}"
        );

        // With the flag AND a policy that allows unsandboxed commands, the
        // sandbox is bypassed — no wrapper appears in the spawned command.
        tool.call(
            json!({"command": "echo hi", "dangerouslyDisableSandbox": true}),
            use_ctx(),
            fresh_tx(),
        )
        .await
        .expect("ok");
        let bypassed = last.lock().unwrap()[2].clone();
        assert!(
            !bypassed.contains("sandbox-exec") && !bypassed.contains("bwrap"),
            "dangerouslyDisableSandbox should bypass the sandbox, got: {bypassed}"
        );
    }

    // ===== BASH-18 `coerceInput` (oracle `Vmm`, 2.1.238 BIN off 294485010) =====

    #[test]
    fn coerce_input_moves_numeric_timeout_ms_into_timeout() {
        let tool = BashTool::new(shell_test_ctx(ok_output()));
        let c = tool
            .coerce_input(&json!({"command": "echo hi", "timeout_ms": 5000}))
            .expect("timeout_ms is coercible");
        assert_eq!(c.shape_class, "timeout_ms");
        assert_eq!(c.input["timeout"], json!(5000));
        assert!(
            c.input.get("timeout_ms").is_none(),
            "the alias key must be dropped"
        );
        // `{...e}` order, then `timeout` appended last (`t.timeout = n`).
        let keys: Vec<&str> = c
            .input
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["command", "timeout"]);
    }

    #[test]
    fn coerce_input_accepts_a_digits_only_string_verbatim() {
        let tool = BashTool::new(shell_test_ctx(ok_output()));
        let c = tool
            .coerce_input(&json!({"command": "echo hi", "timeout_ms": "5000"}))
            .expect("digit string is coercible");
        // `t.timeout = n` moves the value ACROSS AS-IS — it stays a STRING.
        assert_eq!(c.input["timeout"], json!("5000"));
    }

    #[test]
    fn coerce_input_is_none_when_timeout_already_present() {
        let tool = BashTool::new(shell_test_ctx(ok_output()));
        // The oracle never enters the branch, so `timeout_ms` SURVIVES into
        // `safeParse` (and legitimately fails `additionalProperties:false`).
        assert!(tool
            .coerce_input(&json!({"command": "x", "timeout": 1, "timeout_ms": 2}))
            .is_none());
    }

    #[test]
    fn coerce_input_discards_the_copy_when_the_value_is_not_coercible() {
        let tool = BashTool::new(shell_test_ctx(ok_output()));
        // `r.length === 0` ⇒ `null`: the pruned copy is thrown away, so the RAW
        // input (with the stray key) is what the schema sees.
        for bad in [json!("5s"), json!("5.5"), json!(true), json!({})] {
            assert!(
                tool.coerce_input(&json!({"command": "x", "timeout_ms": bad}))
                    .is_none(),
                "non-coercible timeout_ms must not produce a rewrite: {bad}"
            );
        }
    }

    #[test]
    fn coerce_input_is_none_without_the_alias_key() {
        let tool = BashTool::new(shell_test_ctx(ok_output()));
        assert!(tool
            .coerce_input(&json!({"command": "x", "timeout": 1}))
            .is_none());
        // `ni(e)` plain-object guard.
        assert!(tool.coerce_input(&json!("not an object")).is_none());
    }

    // ===== BASH-10 sandbox-override ask (oracle BIN off 294577064) ==========

    fn sandboxing_on_ctx() -> BuiltinToolContext {
        let mut ctx = shell_test_ctx(ok_output());
        ctx.sandbox_available = true;
        ctx.sandbox_runtime.excluded_commands = vec![];
        ctx.sandbox_runtime.allow_unsandboxed_commands = true;
        ctx
    }

    #[tokio::test]
    async fn check_permissions_asks_when_only_the_flag_escapes_the_sandbox() {
        let tool = BashTool::new(sandboxing_on_ctx());
        let result = tool
            .check_permissions(
                &json!({"command": "echo hi", "dangerouslyDisableSandbox": true}),
                &use_ctx(),
            )
            .await;
        match result {
            PermissionResult::Ask { reason, prompt, .. } => {
                assert!(
                    matches!(
                        reason,
                        PermissionDecisionReason::SandboxOverride {
                            reason: SandboxOverrideReason::DangerouslyDisableSandbox
                        }
                    ),
                    "decisionReason must be {{type:'sandboxOverride',reason:'dangerouslyDisableSandbox'}}"
                );
                // Byte-locked oracle copy.
                assert_eq!(prompt.message, "Run outside of the sandbox");
                // BASH-19's hook is what titles the prompt.
                assert_eq!(prompt.title, "Bash");
            }
            other => panic!("expected an Ask, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_permissions_allows_without_the_flag() {
        let tool = BashTool::new(sandboxing_on_ctx());
        assert!(matches!(
            tool.check_permissions(&json!({"command": "echo hi"}), &use_ctx())
                .await,
            PermissionResult::Allow { .. }
        ));
    }

    #[tokio::test]
    async fn check_permissions_allows_when_the_flag_changes_nothing() {
        // Sandboxing unavailable ⇒ `BY(e)` is false with AND without the flag,
        // so `BY({...e,dangerouslyDisableSandbox:!1})` fails and no ask is raised.
        let tool = BashTool::new(shell_test_ctx(ok_output()));
        assert!(matches!(
            tool.check_permissions(
                &json!({"command": "echo hi", "dangerouslyDisableSandbox": true}),
                &use_ctx()
            )
            .await,
            PermissionResult::Allow { .. }
        ));
    }

    #[tokio::test]
    async fn check_permissions_allows_when_the_policy_forbids_unsandboxed_commands() {
        // `areUnsandboxedCommandsAllowed()` false ⇒ the flag is ignored, the
        // command is still wrapped, so `!BY(e)` fails.
        let mut ctx = sandboxing_on_ctx();
        ctx.sandbox_runtime.allow_unsandboxed_commands = false;
        let tool = BashTool::new(ctx);
        assert!(matches!(
            tool.check_permissions(
                &json!({"command": "echo hi", "dangerouslyDisableSandbox": true}),
                &use_ctx()
            )
            .await,
            PermissionResult::Allow { .. }
        ));
    }

    // ===== BASH-19 `userFacingName` (oracle BIN off 294576071) =============

    #[test]
    fn user_facing_name_is_bash_without_the_indicator_env() {
        let _g = shell_env_lock();
        let previous = std::env::var_os("LINGXI_BASH_SANDBOX_SHOW_INDICATOR");
        std::env::remove_var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR");
        let tool = BashTool::new(sandboxing_on_ctx());
        // `if(!e) return "Bash"`.
        let empty_input = tool.user_facing_name_for_input(&Value::Null);
        // Indicator unset ⇒ "Bash" even for a command that WILL be wrapped.
        let sandboxed_input = tool.user_facing_name_for_input(&json!({"command": "echo hi"}));
        match previous {
            Some(value) => std::env::set_var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR", value),
            None => std::env::remove_var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR"),
        }
        assert_eq!(empty_input.as_deref(), Some("Bash"));
        assert_eq!(sandboxed_input.as_deref(), Some("Bash"));
    }

    #[test]
    fn user_facing_name_is_sandboxed_bash_with_the_indicator_env() {
        let _g = shell_env_lock();
        let previous = std::env::var_os("LINGXI_BASH_SANDBOX_SHOW_INDICATOR");
        let tool = BashTool::new(sandboxing_on_ctx());
        // JS truthiness, NOT `isEnvTruthy`: `"0"` is a non-empty string and so
        // ENABLES the indicator.
        std::env::set_var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR", "0");
        let sandboxed = tool.user_facing_name_for_input(&json!({"command": "echo hi"}));
        // A `dangerouslyDisableSandbox` call is NOT wrapped ⇒ plain "Bash".
        let unwrapped = tool.user_facing_name_for_input(
            &json!({"command": "echo hi", "dangerouslyDisableSandbox": true}),
        );
        match previous {
            Some(value) => std::env::set_var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR", value),
            None => std::env::remove_var("LINGXI_BASH_SANDBOX_SHOW_INDICATOR"),
        }
        assert_eq!(sandboxed.as_deref(), Some("SandboxedBash"));
        assert_eq!(unwrapped.as_deref(), Some("Bash"));
    }

    /// A `SandboxRunner` that records every `wrap`/`cleanup_after_command` call
    /// and returns a sentinel-prefixed wrapped command, so a test can prove the
    /// Bash tool routes through `ctx.sandbox_runner` (not the sync free fn) with
    /// the right args and invokes cleanup after the command finishes.
    struct WrapCall {
        command: String,
        bin_shell: Option<String>,
        cwd: Option<std::path::PathBuf>,
    }

    #[derive(Default)]
    struct RecordingSandboxRunner {
        wrap_calls: std::sync::Mutex<Vec<WrapCall>>,
        cleanups: std::sync::atomic::AtomicUsize,
        violations: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl tool_api::SandboxRunner for RecordingSandboxRunner {
        async fn wrap(
            &self,
            command: &str,
            _cfg: &sandbox::runtime_config::SandboxRuntimeConfig,
            _platform: sandbox::runtime_config::Platform,
            bin_shell: Option<&str>,
            cwd: Option<&std::path::Path>,
        ) -> Result<String, sandbox::wrap::SandboxWrapError> {
            self.wrap_calls.lock().unwrap().push(WrapCall {
                command: command.to_string(),
                bin_shell: bin_shell.map(ToString::to_string),
                cwd: cwd.map(std::path::Path::to_path_buf),
            });
            Ok(format!("WRAPPED::{command}"))
        }

        async fn cleanup_after_command(&self) {
            self.cleanups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        async fn command_violations(
            &self,
            _command: &str,
        ) -> tool_api::sandbox_runner::SandboxCommandViolations {
            tool_api::sandbox_runner::SandboxCommandViolations {
                lines: self.violations.lock().unwrap().clone(),
            }
        }
    }

    #[tokio::test]
    async fn sandbox_branch_routes_through_injected_runner_and_cleans_up() {
        let last = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let runner = Arc::new(RecordingSandboxRunner::default());
        let mut ctx = shell_test_ctx(ok_output());
        // Force the Sandbox branch: available sandbox + no excluded commands.
        ctx.sandbox_available = true;
        ctx.sandbox_runtime.excluded_commands = vec![];
        ctx.session_cwd
            .swap(std::path::PathBuf::from("/tmp"), ctx.trusted_dirs());
        ctx.sandbox_runner = runner.clone();
        ctx.process = Arc::new(CapturingRunner {
            out: ok_output(),
            last_args: last.clone(),
        });
        let tool = BashTool::new(ctx);
        tool.call(json!({"command": "echo hi"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");

        // The injected runner was called exactly once, with the resolved shell
        // and the workspace cwd.
        let calls = runner.wrap_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "wrap should be called once");
        let call = &calls[0];
        assert!(
            call.command.contains("echo hi"),
            "runner received the spawn command, got: {}",
            call.command
        );
        assert_eq!(call.bin_shell.as_deref(), Some(resolve_shell_path()));
        assert_eq!(call.cwd.as_deref(), Some(std::path::Path::new("/tmp")));

        // The runner's wrapped output (sentinel) is what actually got spawned.
        let spawned = last.lock().unwrap()[2].clone();
        assert!(
            spawned.contains("WRAPPED::"),
            "the runner's wrapped command must be spawned, got: {spawned}"
        );

        // cleanup_after_command ran after the command finished.
        assert_eq!(
            runner.cleanups.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cleanup_after_command must be invoked once"
        );
    }

    #[tokio::test]
    async fn sandbox_violations_are_appended_to_stderr_even_on_exit_zero() {
        let runner = Arc::new(RecordingSandboxRunner::default());
        runner.violations.lock().unwrap().push(
            "deny network-outbound denied.example:443 (host is not on the allow list)".into(),
        );
        let mut ctx = shell_test_ctx(ok_output());
        ctx.sandbox_available = true;
        ctx.sandbox_runtime.excluded_commands = vec![];
        ctx.session_cwd
            .swap(std::path::PathBuf::from("/tmp"), ctx.trusted_dirs());
        ctx.sandbox_runner = runner;
        let tool = BashTool::new(ctx);
        let res = tool
            .call(json!({"command": "echo hi"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        let stderr = res.data["stderr"].as_str().unwrap();
        assert_eq!(
            stderr,
            "<sandbox_violations>\ndeny network-outbound denied.example:443 (host is not on the allow list)\n</sandbox_violations>"
        );
        assert!(res
            .model_content
            .as_deref()
            .unwrap()
            .contains("<sandbox_violations>"));
    }

    #[tokio::test]
    async fn dangerously_disable_sandbox_ignored_when_policy_disallows() {
        let last = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut ctx = shell_test_ctx(ok_output());
        ctx.sandbox_available = true;
        ctx.sandbox_runtime.excluded_commands = vec![];
        // `allow_unsandboxed_commands = false` ⇒ `areUnsandboxedCommandsAllowed()`
        // is false, so the flag must be ignored and the command stays sandboxed.
        ctx.sandbox_runtime.allow_unsandboxed_commands = false;
        ctx.process = Arc::new(CapturingRunner {
            out: ok_output(),
            last_args: last.clone(),
        });
        let tool = BashTool::new(ctx);
        tool.call(
            json!({"command": "echo hi", "dangerouslyDisableSandbox": true}),
            use_ctx(),
            fresh_tx(),
        )
        .await
        .expect("ok");
        let spawned = last.lock().unwrap()[2].clone();
        assert!(
            spawned.contains("sandbox-exec") || spawned.contains("bwrap"),
            "flag must be ignored when policy disallows unsandboxed cmds, got: {spawned}"
        );
    }

    // ===== Image-output handling (claude-code `BashTool/utils.ts`) ==========

    /// A minimal valid 1x1 transparent PNG, base64-encoded (the payload portion
    /// of a `data:image/png;base64,…` URI).
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    #[test]
    fn is_image_output_mirrors_ts_regex() {
        // Matches `^data:image/[a-z0-9.+_-]+;base64,` (case-insensitive).
        assert!(is_image_output("data:image/png;base64,AAAA"));
        assert!(is_image_output("data:image/jpeg;base64,AAAA"));
        assert!(is_image_output("data:image/svg+xml;base64,AAAA"));
        assert!(is_image_output("data:image/x-icon;base64,AAAA"));
        // `i` flag: uppercase scheme/subtype still matches.
        assert!(is_image_output("DATA:IMAGE/PNG;base64,AAAA"));
        // Non-matches.
        assert!(!is_image_output("hello world"));
        assert!(!is_image_output("data:text/plain;base64,AAAA")); // not image/*
        assert!(!is_image_output("data:image/;base64,AAAA")); // empty subtype
        assert!(!is_image_output("data:image/png;,AAAA")); // missing base64 token
        assert!(!is_image_output("data:image/png")); // no `;base64,`
        assert!(!is_image_output(" data:image/png;base64,AAAA")); // leading space (TS `^`)
    }

    #[test]
    fn parse_data_uri_extracts_media_type_and_payload() {
        let (mt, data) = parse_data_uri("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(mt, "image/png");
        assert_eq!(data, "iVBORw0KGgo=");
        // Trimmed before matching.
        let (mt, data) = parse_data_uri("  data:image/jpeg;base64,QQ==  ").unwrap();
        assert_eq!(mt, "image/jpeg");
        assert_eq!(data, "QQ==");
        // Malformed: missing `;base64,` → None.
        assert!(parse_data_uri("data:image/png;base64").is_none());
        assert!(parse_data_uri("data:image/png,QQ==").is_none());
        // Empty payload → None (TS `(.+)$`).
        assert!(parse_data_uri("data:image/png;base64,").is_none());
        // Not a data URI → None.
        assert!(parse_data_uri("just text").is_none());
    }

    #[tokio::test]
    async fn image_stdout_is_flagged_for_the_tool_result_image_block() {
        // A command whose stdout is a valid `data:image/png;base64,…` URI whose
        // payload SNIFFS as a real image: `data.isImage == true` and the URI
        // stays in `data.stdout` — the dispatch loop's `hKn` port derives the
        // tool_result image block FROM that stdout, so the tool injects NO
        // follow-up message (the image rides INSIDE the tool_result).
        let uri = format!("data:image/png;base64,{TINY_PNG_B64}");
        let out = ProcessOutput {
            stdout: format!("{uri}\n"),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "python plot.py"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");

        // `data` is the main result shape with `isImage: true`; no LingXi-only
        // `type`/`media_type`/`truncated` keys, and the display placeholder rides
        // on `model_content`, not inside `data`.
        assert_eq!(res.data["isImage"], true);
        assert_eq!(res.data["interrupted"], false);
        assert_eq!(
            res.model_content.as_deref(),
            Some("[Image content provided in tool result.]")
        );
        assert!(res.data.get("type").is_none());
        assert!(res.data.get("media_type").is_none());
        assert!(res.data.get("truncated").is_none());
        // `isImage` flags that `stdout` CONTAINS the image: the (untruncated)
        // data-URI rides in `stdout` for the mapper to derive the block from.
        assert_eq!(res.data["stdout"], uri);
        // NO injected message — the image reaches the model inside the
        // tool_result content array, byte-faithful to the binary's `hKn`.
        assert!(res.new_messages.is_empty());
    }

    #[tokio::test]
    async fn oversized_image_stdout_is_resized_before_tool_result_delivery() {
        const WIDE_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAC7gAAAABCAIAAADBtXRpAAAAH0lEQVR42u3BAQEAAACCIP+vbkhAAQAAAAAAAADAgQEjKQABp2QvZgAAAABJRU5ErkJggg==";
        let original = format!("data:image/png;base64,{WIDE_PNG}");
        let tool = BashTool::new(shell_test_ctx(ProcessOutput {
            stdout: format!("{original}\n"),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }));

        let result = tool
            .call(json!({"command": "python plot.py"}), use_ctx(), fresh_tx())
            .await
            .expect("image result");

        let resized = result.data["stdout"].as_str().unwrap();
        assert_ne!(resized, original);
        assert!(resized.starts_with("data:image/jpeg;base64,"));
    }

    #[tokio::test]
    async fn image_uri_with_unsniffable_payload_falls_through_to_text() {
        // A syntactically valid data-URI whose payload is NOT a recognized image
        // (magic sniff fails) must take the normal TEXT path — the binary's
        // `hKn` returns null and the mapper falls through. `aGVsbG8=` = "hello".
        let out = ProcessOutput {
            stdout: "data:image/png;base64,aGVsbG8=\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "echo fake"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(res.data["isImage"], false, "unsniffable payload is TEXT");
        assert!(res.new_messages.is_empty());
    }

    #[tokio::test]
    async fn normal_text_command_is_not_an_image() {
        // A normal text command: `isImage == false`, stdout flows as text, and
        // no image is attached to `new_messages`.
        let out = ProcessOutput {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "echo hello"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(res.data["isImage"], false);
        assert_eq!(res.data["stdout"], "hello");
        assert!(
            res.new_messages.is_empty(),
            "no image message for plain text"
        );
    }

    #[tokio::test]
    async fn malformed_image_uri_is_treated_as_text() {
        // Image-like but malformed (no `;base64,`): NOT an image — flows as
        // normal text and is NOT attached as an image message.
        let bad = "data:image/png;not-base64-here";
        let out = ProcessOutput {
            stdout: format!("{bad}\n"),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let res = tool
            .call(json!({"command": "cat weird.txt"}), use_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(res.data["isImage"], false);
        assert_eq!(res.data["stdout"], bad);
        assert!(res.new_messages.is_empty());
    }

    // ===== BASH.7 — `Dh(model)` prompt gate ================================
    //
    // The `Dh(model)` predicate itself (SHORT/LONG, `UWu`/`dfe` model
    // classification) is unit-tested in `tool-api`'s `model_prompt_gate`; here
    // we lock the Bash tool's observable behavior — that `prompt()` routes the
    // gate to the correct SHORT/LONG variant per `PromptOptions::model`.

    #[tokio::test]
    async fn prompt_default_model_none_returns_long_variant() {
        // Default opts (model=None) ⇒ `Dh(None)` false ⇒ LONG prompt. The
        // byte-locked LONG anchors must still hold (regression guard for the
        // existing parity tests).
        let out = ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let p = tool.prompt(&PromptOptions::default()).await;
        assert!(
            p.starts_with("Executes a given bash command and returns its output."),
            "LONG prompt opening missing; got:\n{p}"
        );
        assert!(
            p.contains(
                "The working directory persists between commands, but shell state does not."
            ),
            "LONG cwd sentence missing"
        );
        assert!(
            p.contains("# Committing changes with git"),
            "LONG committing-changes header missing"
        );
    }

    #[tokio::test]
    async fn prompt_current_gen_model_returns_short_variant() {
        // The SHORT prompt's `run_in_background` bullet is gated on
        // `background_usage_note()`, which reads the process-global
        // `LINGXI_DISABLE_BACKGROUND_TASKS`. A sibling test SETS that var, so
        // without a shared guard this test intermittently rendered a prompt
        // with the bullet missing — it failed roughly once per full-workspace
        // run and passed every time in isolation.
        //
        // The underlying defect was THREE locks over one global — two in this
        // file and one in `prompt.rs` — which is no mutual exclusion at all.
        // They are a single crate-wide lock now, so a `prompt.rs` test setting
        // the var can no longer race a `bash.rs` prompt assertion.
        let _g = crate::prompt::background_env_lock();
        // `model: Some("claude-opus-4-8")` ⇒ `Dh` true ⇒ SHORT prompt — exactly
        // what claude-code serves opus-4-8. Default test sandbox is disabled, so
        // the sandbox section is absent and the git section is the CONCISE one.
        let out = ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let tool = BashTool::new(shell_test_ctx(out));
        let opts = PromptOptions {
            model: Some("claude-opus-4-8".into()),
            ..Default::default()
        };
        let p = tool.prompt(&opts).await;

        // Opening — SHORT ("a bash command", NOT "a given bash command").
        assert!(
            p.starts_with("Executes a bash command and returns its output.\n\n"),
            "SHORT opening missing; got:\n{p}"
        );
        assert!(
            !p.contains("Executes a given bash command"),
            "must NOT be the LONG opening"
        );
        // Working-directory bullet (em-dash U+2014, straight apostrophe).
        assert!(
            p.contains("- Working directory persists between calls, but prefer absolute paths \u{2014} `cd` in a compound command can trigger a permission prompt. Shell state (env vars, functions) does not persist; the shell is initialized from the user's profile."),
            "SHORT working-directory bullet missing/incorrect; got:\n{p}"
        );
        // IMPORTANT avoid-list bullet — claude-code 2.1.238 `hcT` selects it with
        // the SAME `VH()` predicate the LONG builder `Yhm` uses. LingXi ships
        // Glob/Grep as real tools ⇒ the non-embedded (find/grep-INCLUSIVE) branch,
        // identical to the LONG prompt's list.
        assert!(
            p.contains("- IMPORTANT: Avoid using this tool to run `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed"),
            "SHORT avoid-list bullet missing/incorrect; got:\n{p}"
        );
        // Output-visibility bullet — UNCONDITIONAL in 2.1.238 (the 2.1.220
        // `CLAUDE_CODE_MARL_CORMORANT` gate was deleted), so opus-4-8 gets it too.
        assert!(
            p.contains("- Command output is displayed to you, not reliably to the user."),
            "SHORT output-visibility bullet missing; got:\n{p}"
        );
        // Raw timeout bullet (no `/ N minutes` conversion).
        assert!(
            p.contains("- `timeout` is in milliseconds: default 120000, max 600000."),
            "SHORT timeout bullet missing/incorrect; got:\n{p}"
        );
        // Detached run_in_background bullet (background note enabled by default,
        // no Monitor clause since the amber-sentinel gate is default-false).
        assert!(
            p.contains("- `run_in_background` runs the command detached: it keeps running across turns and re-invokes you when it exits. No `&` needed."),
            "SHORT run_in_background bullet missing; got:\n{p}"
        );
        assert!(
            !p.contains("Foreground `sleep` is blocked"),
            "Monitor clause must be absent by default (amber-sentinel off)"
        );
        // CONCISE `# Git` section — three fixed bullets, NO attribution bullets,
        // NO LONG "Committing changes with git" header.
        assert!(
            p.contains("# Git\n- Interactive flags (`-i`, e.g. `git rebase -i`, `git add -i`) are not supported in this environment.\n- Use the `gh` CLI for GitHub operations (PRs, issues, API).\n- Commit or push only when the user asks. If on the default branch, branch first."),
            "SHORT `# Git` section missing/incorrect; got:\n{p}"
        );
        assert!(
            !p.contains("# Committing changes with git"),
            "SHORT prompt must not carry the LONG git section"
        );
        // BASH-14: the SHORT `# Git` section DOES carry the attribution
        // bullets. The oracle builds them as
        //   [r?`- End git commit messages with:\n${r}`:null,
        //    o?`- End PR bodies with:\n${o}`:null].filter(Boolean).join("\n")
        // so they appear whenever the attribution texts are non-empty — and
        // `attribution_texts()` returns the DEFAULT pair, so they are.
        //
        // Confirmed against a live 2.1.238 session, whose concise `# Git`
        // section is exactly the three bullets asserted above followed by
        // "- End git commit messages with:" / "- End PR bodies with:".
        // The old assertion asserted their ABSENCE on the premise that there is
        // "no attribution source"; that premise stopped holding once the
        // attribution slots were wired.
        assert!(
            p.contains("- End git commit messages with:") && p.contains("- End PR bodies with:"),
            "SHORT prompt must carry both attribution bullets; got:\n{p}"
        );
        // Sandbox section absent (default test sandbox disabled).
        assert!(
            !p.contains("## Command sandbox"),
            "sandbox section should be absent when sandbox disabled"
        );
    }
    #[tokio::test]
    async fn background_request_waits_for_process_detachment_before_host_cancel() {
        use platform_api::task_registry::{TaskBackgroundReason, TaskBackgrounder};
        let notify = Arc::new(tokio::sync::Notify::new());
        let reason = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let settled = Arc::new(tokio::sync::watch::channel(false).0);
        let requester = Arc::new(BackgroundBashRequester {
            notify: notify.clone(),
            reason: reason.clone(),
            settled: settled.clone(),
        });
        let task = tokio::spawn(async move {
            requester
                .background_with_reason(TaskBackgroundReason::DeliverMessage)
                .await;
        });
        notify.notified().await;
        assert!(
            !task.is_finished(),
            "the host may not cancel before the shell has detached"
        );
        assert_eq!(
            reason.load(std::sync::atomic::Ordering::SeqCst),
            TaskBackgroundReason::DeliverMessage as u8
        );
        drop(BackgroundRequestSettled(settled));
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn background_heads_preserve_manual_and_message_triggers() {
        use platform_api::task_registry::TaskBackgroundReason;
        let manual = background_note_for_reason(
            "b1",
            "/tmp/b1.output",
            None,
            false,
            TaskBackgroundReason::User as u8,
        );
        let message = background_note_for_reason(
            "b1",
            "/tmp/b1.output",
            None,
            false,
            TaskBackgroundReason::DeliverMessage as u8,
        );
        assert!(manual.starts_with("Command was manually backgrounded by user with ID: b1."));
        assert!(message.contains("so that a message that arrived while it was running can reach you; it was not interrupted."));
        assert!(!manual.contains("timeout") && !message.contains("timeout"));
    }
    #[tokio::test]
    async fn turn_abort_before_foreground_arming_detaches_the_same_runner() {
        let _gate = crate::prompt::background_env_lock();
        struct AbortRunner(Arc<tokio::sync::Notify>, bool);
        #[async_trait::async_trait]
        impl ProcessRunner for AbortRunner {
            fn supports_foreground_backgrounding(&self) -> bool {
                self.1
            }
            async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
                unreachable!()
            }
            async fn run_foreground_with_output_limit(
                &self,
                cmd: &SandboxedCommand,
                _: Option<usize>,
            ) -> Result<platform_api::ForegroundRunResult, ProcessError> {
                let binding = cmd.background_task().expect("bound identity");
                self.0.notify_one();
                if let Some(notify) = binding.on_demand.as_ref() {
                    notify.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
                Ok(platform_api::ForegroundRunResult {
                    outcome: platform_api::ForegroundOutcome::MovedToBackground(ProcessHandle {
                        task_id: binding.task_id.clone(),
                        pid: 4242,
                    }),
                    output_file: None,
                })
            }
            async fn spawn_background(
                &self,
                _: &SandboxedCommand,
            ) -> Result<ProcessHandle, ProcessError> {
                panic!("must preserve the foreground runner");
            }
            async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
                panic!("abort must not kill the preserved process");
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        for supports_detach in [false, true] {
            let registry = Arc::new(RecordingRegistry::default());
            let started = Arc::new(tokio::sync::Notify::new());
            let mut context = shell_test_ctx(ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            });
            context.task_registry = Some(registry.clone());
            context.process = Arc::new(AbortRunner(started.clone(), supports_detach));
            let tool = BashTool::new(context);
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut use_context = use_ctx();
            use_context.cancel = Some(cancel.clone());
            let run = tokio::spawn(async move {
                tool.call(json!({"command":"sleep 10"}), use_context, fresh_tx())
                    .await
            });
            started.notified().await;
            cancel.cancel();
            let outcome = tokio::time::timeout(Duration::from_secs(1), run)
                .await
                .unwrap()
                .unwrap();
            if !supports_detach {
                assert!(matches!(outcome, Err(ToolError::Aborted)));
                continue;
            }
            let result = outcome.expect("detached result");
            assert_eq!(result.data["backgroundedByTurnAbort"], true);
            assert!(result.data.get("timedOutAfterMs").is_none());
            assert_eq!(result.data["backgroundTaskId"], "b1a2b3c4d");
            assert!(
                registry.armed.lock().unwrap().is_empty(),
                "test must exercise the pre-2s path"
            );
        }
    }

    // ===== CLI-5 `bashEditDiffEnabled` wiring =============================

    /// A `ProcessRunner` that actually CHANGES the worktree, so the before and
    /// after snapshots genuinely differ.
    ///
    /// The ordinary `StubProcess` returns canned output and touches nothing, so
    /// every snapshot it produces is identical and no diff can ever appear —
    /// a test built on it would pass with the whole feature removed.
    struct EditingProcess {
        write: std::path::PathBuf,
    }

    #[async_trait]
    impl platform_api::process::ProcessRunner for EditingProcess {
        async fn run(
            &self,
            _cmd: &platform_api::sandbox::SandboxedCommand,
        ) -> Result<platform_api::process::ProcessOutput, platform_api::process::ProcessError>
        {
            std::fs::write(&self.write, "written by the command\n").unwrap();
            Ok(platform_api::process::ProcessOutput {
                stdout: "done\n".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }

        async fn spawn_background(
            &self,
            _cmd: &platform_api::sandbox::SandboxedCommand,
        ) -> Result<platform_api::process::ProcessHandle, platform_api::process::ProcessError>
        {
            Err(platform_api::process::ProcessError::Unsupported)
        }

        async fn kill(
            &self,
            _handle: &platform_api::process::ProcessHandle,
        ) -> Result<(), platform_api::process::ProcessError> {
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    fn git_in(cwd: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
    }

    /// A repo, a `BashTool` whose context has CLI-5 on, and a `ToolUseContext`
    /// rooted in that repo.
    fn edit_diff_fixture() -> (
        std::sync::RwLockReadGuard<'static, ()>,
        tempfile::TempDir,
        BashTool,
        ToolUseContext,
    ) {
        // The `powershell` tests in this binary replace PATH; `git` is resolved
        // from it here and inside `bash_edit_diff`. See `crate::test_path_env`.
        let path_guard = crate::test_path_env::read();
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["config", "user.email", "t@example.invalid"]);
        git_in(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("kept.txt"), "one\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-qm", "seed"]);

        let mut bctx = tool_api::test_support::shell_test_ctx_in(
            ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
            repo.clone(),
        );
        bctx.process = Arc::new(EditingProcess {
            write: repo.join("touched.txt"),
        });
        bctx.bash_edit_diff = Some(Arc::new(tool_api::BashEditDiffSetup {
            shadow_root: tmp.path().join("shadow"),
        }));
        let tool = BashTool::new(bctx);
        let mut ctx = use_ctx();
        ctx.cwd = Some(repo);
        (path_guard, tmp, tool, ctx)
    }

    /// The MODEL-facing render, which is where the diff lands — not `data`,
    /// which is the TUI / `PostToolUse` metadata object.
    fn rendered(res: &ToolCallResult) -> String {
        format!("{:?} {:?}", res.model_content, res.data)
    }

    /// 🚨 HP-6: `sandbox.credentials` must hold on the UNSANDBOXED path.
    ///
    /// The withholding used to live only in the sandbox wrap, so a command that
    /// `should_use_sandbox` sent down the `NoSandbox` arm — an excluded command,
    /// `allowUnsandboxedCommands`, or `dangerouslyDisableSandbox` — received the
    /// real credential while the setting reported it protected. That is a
    /// security boundary silently conditioned on an unrelated decision.
    #[tokio::test]
    async fn a_credential_env_var_is_dropped_even_when_the_command_is_not_sandboxed() {
        let mut bctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        // No sandbox available ⇒ `should_use_sandbox` takes the NoSandbox arm.
        bctx.sandbox_available = false;
        bctx.sandbox_runtime.credential_deny_env = vec!["AWS_SECRET_ACCESS_KEY".to_string()];
        let recorder = Arc::new(RecordingProcess::default());
        bctx.process = recorder.clone();

        let tool = BashTool::new(bctx);
        let _ = tool
            .call(
                json!({"command": "printenv AWS_SECRET_ACCESS_KEY"}),
                use_ctx(),
                fresh_tx(),
            )
            .await;

        let seen = recorder.commands.lock().unwrap().join("\n");
        assert!(
            seen.contains("unset AWS_SECRET_ACCESS_KEY"),
            "the credential must be dropped on the unsandboxed path too, got: {seen}"
        );
    }

    /// Records the command strings handed to the process runner.
    #[derive(Default)]
    struct RecordingProcess {
        commands: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl platform_api::process::ProcessRunner for RecordingProcess {
        async fn run(
            &self,
            cmd: &platform_api::sandbox::SandboxedCommand,
        ) -> Result<platform_api::process::ProcessOutput, platform_api::process::ProcessError>
        {
            self.commands.lock().unwrap().push(format!("{cmd:?}"));
            Ok(platform_api::process::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }

        async fn spawn_background(
            &self,
            _cmd: &platform_api::sandbox::SandboxedCommand,
        ) -> Result<platform_api::process::ProcessHandle, platform_api::process::ProcessError>
        {
            Err(platform_api::process::ProcessError::Unsupported)
        }

        async fn kill(
            &self,
            _handle: &platform_api::process::ProcessHandle,
        ) -> Result<(), platform_api::process::ProcessError> {
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    /// The end-to-end wiring: a command that changed a file reports the diff.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_command_that_changed_a_file_reports_the_diff() {
        let (_path, _tmp, tool, ctx) = edit_diff_fixture();
        let res = tool
            .call(
                json!({"command": "sh -c 'touch touched.txt'"}),
                ctx,
                fresh_tx(),
            )
            .await
            .expect("call");
        let text = rendered(&res);
        assert!(
            text.contains("Files changed by this command:"),
            "the diff section is missing from the tool result: {text}"
        );
        assert!(
            text.contains("touched.txt"),
            "the diff must name the file the command wrote: {text}"
        );
    }

    /// `!run_in_background` — a backgrounded command has no "after" yet, so it
    /// must not even pay for the BEFORE snapshot.
    ///
    /// 🚨 Asserting "the result carries no diff" would be vacuous here: the
    /// background arm returns through a different builder that never appends
    /// one, so that assertion holds with the conjunct deleted. The observable
    /// consequence of the conjunct is that no shadow repository is created at
    /// all — pinned against a foreground control that proves the fixture does
    /// create one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_backgrounded_command_never_takes_a_snapshot() {
        let shadow_of = |tool: &BashTool| {
            tool.ctx
                .bash_edit_diff
                .as_ref()
                .expect("the fixture wires CLI-5")
                .shadow_root
                .clone()
        };

        let (_path, _tmp, tool, ctx) = edit_diff_fixture();
        let shadow = shadow_of(&tool);
        let _ = tool
            .call(
                json!({"command": "sh -c 'touch touched.txt'", "run_in_background": true}),
                ctx,
                fresh_tx(),
            )
            .await;
        assert!(
            !shadow.exists(),
            "a backgrounded command must not snapshot the worktree"
        );

        let (_path2, _tmp2, tool2, ctx2) = edit_diff_fixture();
        let shadow2 = shadow_of(&tool2);
        let _ = tool2
            .call(
                json!({"command": "sh -c 'touch touched.txt'"}),
                ctx2,
                fresh_tx(),
            )
            .await
            .expect("call");
        assert!(
            shadow2.exists(),
            "…and the same fixture DOES snapshot in the foreground, so the \
             assertion above is about `run_in_background` and not about the \
             fixture being unable to snapshot at all"
        );
    }

    /// `!SNt(input)` — a command permission classifies as READ-ONLY cannot have
    /// changed a file, so snapshotting it is pure cost.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_read_only_command_is_never_diffed() {
        let (_path, _tmp, tool, ctx) = edit_diff_fixture();
        assert!(
            tool.is_read_only(&json!({"command": "ls"})),
            "`ls` must classify as read-only, or this test proves nothing"
        );
        let res = tool
            .call(json!({"command": "ls"}), ctx, fresh_tx())
            .await
            .expect("call");
        assert!(
            !rendered(&res).contains("Files changed by this command:"),
            "a read-only command must not be diffed"
        );
    }

    /// `!_0r(command)` — a bare `git checkout` moves the whole worktree, and
    /// its diff would describe the branch rather than the command.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_branch_moving_git_command_is_never_diffed() {
        let (_path, _tmp, tool, ctx) = edit_diff_fixture();
        let res = tool
            .call(json!({"command": "git checkout main"}), ctx, fresh_tx())
            .await
            .expect("call");
        assert!(
            !rendered(&res).contains("Files changed by this command:"),
            "a bare branch-moving git call must not be diffed"
        );
    }

    /// PRESENCE is the gate: a host that never wires the setup behaves exactly
    /// as it did before CLI-5 existed.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_unwired_host_reports_no_diff_at_all() {
        let (_path, _tmp, mut tool, ctx) = edit_diff_fixture();
        tool.ctx.bash_edit_diff = None;
        let res = tool
            .call(
                json!({"command": "sh -c 'touch touched.txt'"}),
                ctx,
                fresh_tx(),
            )
            .await
            .expect("call");
        assert!(!rendered(&res).contains("Files changed by this command:"));
    }
}
