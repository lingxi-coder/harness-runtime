//! `GrepTool` — ripgrep-faithful content search.
//!
//! 1:1 port of claude-code's `GrepTool.ts`. claude-code shells out to the
//! bundled `rg` binary; this Rust port keeps an **in-process** engine built on
//! the `ignore` + `grep_*` crates (which is what ripgrep itself is built on),
//! so flag parity is *behavioral*, not process-identical. The model-facing
//! result is mapped from structured data to a model-facing string.
//!
//! Documented divergences from the `rg` CLI (behavioral parity, not blockers):
//! - Engine is `ignore::WalkBuilder` + `grep_searcher`, not the `rg` process.
//! - `--type` coverage is whatever `ignore::types::TypesBuilder::add_defaults`
//!   ships — may be a sub/superset of the bundled `rg` type list.
//! - Context lines (`-A`/`-B`/`-C`) are gathered via a custom `Sink`; they are
//!   relativized and use the same `relpath:line:text` field separator as match
//!   lines (the `rg` CLI uses `-` separators + keeps absolute paths for context
//!   lines because of a TS relativize quirk). No `--` group separators emitted.
//! - `--max-columns 500` is approximated by hard-truncating emitted text to 500
//!   chars (`rg` prints an omission marker instead).
//! - gitignore-relative `--glob` negation nuance (`GrepTool.ts:417-427`) is
//!   approximated via `ignore::overrides::OverrideBuilder`.
//! - mtime-desc file sort falls back to a pure filename sort under `cfg!(test)`
//!   (mirrors TS `NODE_ENV === 'test'`).
//! - There is NO per-file match cap (matching TS / the `rg` CLI): count mode
//!   reports the TRUE per-file + total counts, and content mode is bounded only
//!   by `head_limit` (default 250). A large `RECORDS_CAP` (10_000) bounds the
//!   *recorded* lines per file as a memory safety valve, but never limits the
//!   *count* — so totals stay accurate even when recording stops.
//! - A wall-clock walk budget honors `LINGXI_GLOB_TIMEOUT_SECONDS`
//!   (default 20s, 60s on WSL); a timeout with zero results is surfaced as an
//!   error (`utils/ripgrep.ts:130-133,444-454`), partial results are returned.
//! - File-read ignore-patterns (`GrepTool.ts:411-427`) are WIRED: the active
//!   `Read`-`deny` permission rules are resolved to `--glob` excludes at engine
//!   boot by `permission::read_deny_exclude_globs` (1:1 port of
//!   `F4e(U4e(toolPermissionContext), cwd)`), threaded in via
//!   `BuiltinToolContext::read_deny_exclude_globs`, and applied here as negated
//!   `ignore`-crate overrides with the reference prefixing (rooted `/P` → `!P`;
//!   relative `P` → `!**/P`). Empty (no `Read`-deny rule) ⇒ VCS excludes only.

use async_trait::async_trait;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, translate_model_path,
};
use tool_api::BuiltinToolContext;

use crate::shared::{
    open_rooted_search_file, run_search_candidate_hook, run_search_preparation_hook,
    SearchResolutionError, SearchResolutionSnapshot,
};

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "Grep";

/// Memory safety valve: cap on the number of *recorded* (content-mode) lines
/// per file. This bounds peak memory for a pathological single-file match storm
/// WITHOUT capping the *count* — `head_limit` (default 250) is the real,
/// TS-faithful truncation, and count mode always reports the true total. Set
/// generously so it never trips for realistic inputs (TS / the `rg` CLI have no
/// per-file cap at all).
pub const GREP_RECORDS_CAP: usize = 10_000;

/// Default cap on grep results when `head_limit` is unspecified
/// (`GrepTool.ts:108`). Pass `head_limit=0` for unlimited.
const DEFAULT_HEAD_LIMIT: usize = 250;

/// VCS directories excluded from searches (`GrepTool.ts:95-102`).
const VCS_DIRECTORIES_TO_EXCLUDE: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// `--max-columns 500` equivalent (`GrepTool.ts:338`).
const MAX_COLUMNS: usize = 500;

/// `Uve.maxResultSizeChars` = `20000` (binary 2.1.238 @289933813). Grep's own
/// cap — five times smaller than Glob's, and not the shared default.
const GREP_MAX_RESULT_SIZE_CHARS: usize = 20_000;

/// Model-facing description / prompt (`prompt.ts:6-18`, byte-faithful with
/// `GREP_TOOL_NAME='Grep'`, `AGENT_TOOL_NAME='Agent'`, `BASH_TOOL_NAME='Bash'`).
/// Everything down to the "Output modes:" bullet — the gated Agent bullet and
/// the tail live in [`GREP_DESCRIPTION_AGENT_BULLET`] / [`GREP_DESCRIPTION_TAIL`]
/// (ST-03).
const GREP_DESCRIPTION_HEAD: &str = r#"A powerful search tool built on ripgrep

  Usage:
  - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` through a registered shell tool. The Grep tool has been optimized for correct permissions and access.
  - Supports full regex syntax (e.g., "log.*Error", "function\s+\w+")
  - Filter files with glob parameter (e.g., "*.js", "**/*.tsx") or type parameter (e.g., "js", "py", "rust")
  - Output modes: "content" shows matching lines, "files_with_matches" shows only file paths (default), "count" shows match counts
"#;

/// ST-03: the oracle wraps this ONE line in `${DZ()==="default"?…:""}` (2.1.238
/// `Wka`, source text @286224735) — under a non-default subagent steer the whole
/// line, newline included, disappears from the middle of the bullet list. Port
/// gate: `lingxi_core::host::live_sessions::subagent_steer_is_default()`.
const GREP_DESCRIPTION_AGENT_BULLET: &str =
    "  - Use Agent tool (if available) for open-ended searches requiring multiple rounds\n";

/// The bullets that follow the gated Agent line.
const GREP_DESCRIPTION_TAIL: &str = r#"  - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping (use `interface\{\}` to find `interface{}` in Go code)
  - Multiline matching: By default patterns match within single lines only. For cross-line patterns like `struct \{[\s\S]*?field`, use `multiline: true`
"#;

/// `Wka(e)`'s long arm, with the Agent bullet gated exactly as upstream.
fn grep_description(steer_is_default: bool) -> String {
    let bullet = if steer_is_default {
        GREP_DESCRIPTION_AGENT_BULLET
    } else {
        ""
    };
    format!("{GREP_DESCRIPTION_HEAD}{bullet}{GREP_DESCRIPTION_TAIL}")
}

/// The SHORT Grep prompt — byte-locked VERBATIM to claude-code `Ajr(e)`'s
/// `Dh(e)===true` branch (binary offset 197076712), served to current-gen
/// default models. `${ns}`=Bash; the two em-dashes are U+2014. The regex
/// snippets render with single backslashes (`function\s+\w+`, `interface\{\}`)
/// — the JS template literal's `\\` collapse to one `\` in the final string.
const GREP_PROMPT_SHORT: &str = r#"Content search built on ripgrep. Prefer this over `grep`/`rg` via a registered shell tool — results integrate with the permission UI and file links.

- Full regex syntax (e.g. "log.*Error", "function\s+\w+"). Ripgrep, not grep — escape literal braces (`interface\{\}`).
- Filter with `glob` (e.g. "**/*.tsx") or `type` (e.g. "js", "py", "rust").
- `output_mode`: "content" (matching lines), "files_with_matches" (paths only, default), or "count".
- `multiline: true` for patterns that span lines."#;

/// `applyHeadLimit` (`GrepTool.ts:110-128`). `limit==Some(0)` → unlimited
/// escape hatch; otherwise slice `[offset, offset+limit)` and report the
/// applied limit only when truncation actually occurred.
fn apply_head_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: usize,
) -> (Vec<T>, Option<usize>) {
    // Explicit 0 = unlimited escape hatch.
    if limit == Some(0) {
        return (items.into_iter().skip(offset).collect(), None);
    }
    let effective = limit.unwrap_or(DEFAULT_HEAD_LIMIT);
    let len = items.len();
    let sliced: Vec<T> = items.into_iter().skip(offset).take(effective).collect();
    // Only report the applied limit when truncation actually occurred, so the
    // model knows there may be more results and can paginate with offset.
    let was_truncated = len.saturating_sub(offset) > effective;
    (sliced, if was_truncated { Some(effective) } else { None })
}

/// Approximate JS `String.prototype.localeCompare(b)` under the default
/// (en-US root) collation, used for the equal-mtime filename tiebreak in
/// `files_with_matches` (binary @200995124: `P[0].localeCompare(L[0])`). The
/// observable divergence from Rust byte `Ord` is CASE: localeCompare orders
/// letters case-insensitively at the PRIMARY level (so `a` < `B` < `c`, unlike
/// byte Ord where `B`(66) < `a`(97)), and breaks an otherwise-equal compare by
/// case at the tertiary level with LOWERCASE before uppercase (`a` < `A`,
/// `readme` < `README`). Verified against `node`:
///   "a".localeCompare("B")===-1, "a".localeCompare("A")===-1,
///   "README".localeCompare("readme")===1, "_x".localeCompare("ax")===-1,
///   "file2".localeCompare("file10")===1 (NOT numeric).
///
/// Implementation: a two-level compare — primary is the lowercased codepoint
/// order (case-insensitive); on a primary tie, the first position whose chars
/// differ (necessarily only by case) is decided lowercase-first. This is exact
/// for the ASCII filename domain (letters/digits/`._-`); full ICU/DUCET
/// weighting for arbitrary Unicode is not ported (no collation dep) — a
/// documented approximation for the rare equal-mtime non-ASCII tiebreak.
fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Primary: case-insensitive (lowercased) codepoint order.
    let primary = a.to_lowercase().cmp(&b.to_lowercase());
    if primary != Ordering::Equal {
        return primary;
    }
    // Tertiary tiebreak: at the first differing char (which can only differ by
    // case given the primary tie), lowercase sorts before uppercase.
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            match (ca.is_lowercase(), cb.is_lowercase()) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => return ca.cmp(&cb),
            }
        }
    }
    a.chars().count().cmp(&b.chars().count())
}

/// `rg -o` / `--only-matching`: the matched substrings within `text`, in order
/// (a line with N matches yields N entries). Uses the SAME `grep_regex` matcher
/// the search ran with, so the extracted spans are byte-identical to ripgrep's.
fn only_matching_spans(matcher: &RegexMatcher, text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at <= bytes.len() {
        match matcher.find_at(bytes, at) {
            Ok(Some(m)) => {
                if let Ok(s) = std::str::from_utf8(&bytes[m.start()..m.end()]) {
                    out.push(s.to_string());
                }
                // Advance past the match; bump zero-width matches by one to
                // avoid looping on the same position.
                at = m.end();
                if m.start() == m.end() {
                    at += 1;
                }
            }
            _ => break,
        }
    }
    out
}

/// `formatLimitInfo` (`GrepTool.ts:134-142`). `appliedLimit` is only set when
/// truncation occurred, so build parts conditionally.
fn format_limit_info(applied_limit: Option<usize>, applied_offset: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(l) = applied_limit {
        parts.push(format!("limit: {l}"));
    }
    if applied_offset > 0 {
        parts.push(format!("offset: {applied_offset}"));
    }
    parts.join(", ")
}

/// Claude Code 2.1.287 `Grep.mapToolResultToToolResultBlockParam`. The data's
/// `content` is the unadorned search output; pagination and summary text are
/// produced here for both ordinary results and Mod replacements.
fn map_grep_result_text(data: &Value) -> Option<String> {
    let limit = data
        .get("appliedLimit")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    let offset = data
        .get("appliedOffset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let pagination = format_limit_info(limit, offset);
    let content = data.get("content").and_then(Value::as_str).unwrap_or("");
    match data
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("files_with_matches")
    {
        "content" => {
            let body = if !content.is_empty() {
                content
            } else if offset > 0 && data.get("totalLines").and_then(Value::as_u64).unwrap_or(0) > 0
            {
                "No entries at this offset"
            } else {
                "No matches found"
            };
            Some(if pagination.is_empty() {
                body.to_string()
            } else {
                format!("{body}\n\n[Showing results with pagination = {pagination}]")
            })
        }
        "count" => {
            let matches = data.get("numMatches").and_then(Value::as_u64).unwrap_or(0);
            let files = data.get("numFiles").and_then(Value::as_u64).unwrap_or(0);
            let body = if !content.is_empty() {
                content
            } else if matches > 0 {
                "No entries at this offset"
            } else {
                "No matches found"
            };
            let suffix = if pagination.is_empty() {
                String::new()
            } else {
                format!(" with pagination = {pagination}")
            };
            Some(format!(
                "{body}\n\nFound {matches} total {} across {files} {}.{suffix}",
                if matches == 1 {
                    "occurrence"
                } else {
                    "occurrences"
                },
                if files == 1 { "file" } else { "files" },
            ))
        }
        _ => {
            let count = data.get("numFiles")?.as_u64()?;
            if count == 0 {
                return Some(
                    if offset > 0 && data.get("totalFiles").and_then(Value::as_u64).unwrap_or(0) > 0
                    {
                        format!("No entries at this offset. [Showing results with pagination = {pagination}]")
                    } else {
                        "No files found".to_string()
                    },
                );
            }
            let filenames = data
                .get("filenames")?
                .as_array()?
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?;
            let suffix = if pagination.is_empty() {
                String::new()
            } else {
                format!(" {pagination}")
            };
            Some(format!(
                "Found {count} {}{suffix}\n{}",
                if count == 1 { "file" } else { "files" },
                filenames.join("\n")
            ))
        }
    }
}

/// `plural` (`stringUtils.ts:32-38`).
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// `toRelativePath` (`path.ts:95-99`): relative to `cwd` if `cwd` is an ancestor,
/// else keep absolute (TS keeps absolute when `relative()` would start with `..`).
///
/// `pub(crate)` so the sibling `glob` module reuses the exact same relativize
/// rule (GLOB.1) instead of duplicating it.
pub(crate) fn to_relative_path(abs: &Path, cwd: &Path) -> String {
    match abs.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => abs.to_string_lossy().into_owned(),
    }
}

/// `isEnvTruthy(process.env[name] || default_str)` (`envUtils.ts:32-37` +
/// `glob.ts:97-99` call form). The env var is read; if unset OR empty, the
/// `default` is used as the value; the result is truthy iff (lower+trim) is one
/// of `1`/`true`/`yes`/`on`. Mirrors TS's `||` (not `??`) so an *empty* env
/// string also falls back to the default.
///
/// `pub(crate)` so the sibling `glob` module reuses the exact same toggle rule.
pub(crate) fn is_env_truthy(name: &str, default: bool) -> bool {
    let raw = std::env::var(name).unwrap_or_default();
    let effective = if raw.is_empty() {
        // `... || 'true'`: empty/unset falls back to the default's string form.
        if default {
            "true".to_string()
        } else {
            "false".to_string()
        }
    } else {
        raw
    };
    matches!(
        effective.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Walk wall-clock budget (`utils/ripgrep.ts:130-133`):
/// `LINGXI_GLOB_TIMEOUT_SECONDS` (parsed as integer seconds, >0) overrides;
/// otherwise the platform default of 20s (60s on WSL, which has a 3-5x file-read
/// penalty). `pub(crate)` so `glob` shares the identical budget.
pub(crate) fn ripgrep_timeout(is_wsl: bool) -> Duration {
    let default_secs = if is_wsl { 60 } else { 20 };
    let secs = std::env::var("LINGXI_GLOB_TIMEOUT_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

/// Verbatim `RipgrepTimeoutError` message (`utils/ripgrep.ts:447-450`). The
/// `{20|60}` second figure follows the platform default. `pub(crate)` for reuse.
#[allow(non_snake_case)]
pub(crate) fn RIPGREP_TIMEOUT_MSG(is_wsl: bool) -> String {
    let secs = if is_wsl { 60 } else { 20 };
    format!(
        "Ripgrep search timed out after {secs} seconds. The search may have matched files but did not complete in time. Try searching a more specific path or pattern."
    )
}

/// Split the `glob` parameter the way `GrepTool.ts:391-409` does: split on
/// whitespace, keep brace-groups intact, otherwise split on commas.
fn split_glob_patterns(glob: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in glob.split_whitespace() {
        if raw.contains('{') && raw.contains('}') {
            out.push(raw.to_string());
        } else {
            for p in raw.split(',') {
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// Decode an emitted line: lossy-UTF8, strip one trailing CRLF. The
/// `--max-columns` rule is applied later, at emit time — see
/// [`apply_max_columns`] (ST-13).
fn decode_line(bytes: &[u8]) -> String {
    let cow = String::from_utf8_lossy(bytes);
    let s = cow.as_ref();
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\r').unwrap_or(s);
    s.to_string()
}

/// ST-13 — the real `--max-columns 500` behaviour. The oracle pushes
/// `"--max-columns","500"` (`Nhv`, @289928728) and ripgrep 14.1.1 does NOT
/// truncate an over-long line: it REPLACES the text with a marker, a different
/// one for matching vs. context lines. Verified against the ripgrep the oracle
/// ships (`ARGV0=rg ~/.local/bin/claude -n --max-columns 500 …`):
///
/// ```text
/// 1:short MATCH here
/// 2:[Omitted long matching line]
/// 1-[Omitted long context line]
/// ```
///
/// The port used to slice the line to its first 500 CHARS, which both invented
/// text ripgrep never prints and (for multi-byte input) applied the wrong
/// threshold — rg measures BYTES, including the line terminator, so a 500-byte
/// line with its newline is 501 and is omitted while a 499-byte one is kept.
/// Under `-o` the rule applies to each emitted MATCH, not to its source line
/// (`rg -n -o --max-columns 500 MATCH` on a 1200-byte line whose match is at
/// column 900 prints `1:MATCH`), which is why callers pass the emitted text.
fn apply_max_columns(text: String, is_context: bool) -> String {
    if text.len() < MAX_COLUMNS {
        return text;
    }
    if is_context {
        "[Omitted long context line]".to_string()
    } else {
        "[Omitted long matching line]".to_string()
    }
}

/// ST-07 — 1:1 port of the `head_limit`/`offset` guard in the oracle's Grep
/// `validateInput` (2.1.238 @289934948; 2.1.220 byte-identical):
///
/// ```js
/// for(let[a,l]of[["head_limit",o],["offset",i]])
///   if(l!==void 0&&(!Number.isInteger(l)||l<0))
///     return{result:!1,message:`${a} must be a whole number of 0 or more, got ${l}.${a==="head_limit"?" Pass 0 for unlimited.":""}`,errorCode:2};
/// ```
///
/// `!==void 0` is "key present" — an explicit `null`, a fraction and a negative
/// all fail. The value is first run through the schema's `ece` preprocess
/// (`$Wr`: trim, `/^[-+]?\d+(\.\d+)?$/` → `Number`), so `"3"` is a valid 3
/// while `"3.5"` is a rejected 3.5 and `"abc"` stays a string. The rendered
/// value follows JS `${l}` (`1.5` → `1.5`, `null` → `null`, a string → itself).
fn validate_whole_number(name: &str, value: Option<&Value>) -> Result<(), ValidationError> {
    let Some(value) = value else { return Ok(()) };
    // `$Wr` — the `ece(...)` preprocess: a numeric string becomes a number.
    let coerced: Option<f64> = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            let numeric = {
                let body = t
                    .strip_prefix('-')
                    .or_else(|| t.strip_prefix('+'))
                    .unwrap_or(t);
                let (int_part, frac_part) = match body.split_once('.') {
                    Some((i, f)) => (i, Some(f)),
                    None => (body, None),
                };
                !int_part.is_empty()
                    && int_part.bytes().all(|b| b.is_ascii_digit())
                    && frac_part
                        .is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
            };
            if numeric {
                t.parse::<f64>().ok().filter(|f| f.is_finite())
            } else {
                None
            }
        }
        _ => None,
    };
    if coerced.is_some_and(|f| f.fract() == 0.0 && f >= 0.0) {
        return Ok(());
    }
    // JS `${l}` rendering of the ORIGINAL (post-preprocess) value.
    let rendered = match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => match coerced {
            // The preprocess replaced the string with a number, so `${l}`
            // prints the number.
            Some(f) => render_js_number(f),
            None => s.clone(),
        },
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), render_js_number),
        other => other.to_string(),
    };
    let hint = if name == "head_limit" {
        " Pass 0 for unlimited."
    } else {
        ""
    };
    Err(ValidationError(format!(
        "{name} must be a whole number of 0 or more, got {rendered}.{hint}"
    )))
}

/// JS `${n}` for a finite number: integers print without a `.0` tail.
fn render_js_number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        format!("{f}")
    }
}

/// `semanticNumber`-ish coercion: accept JSON number or numeric string.
fn value_as_usize(v: &Value) -> Option<usize> {
    if let Some(n) = v.as_u64() {
        return Some(n as usize);
    }
    if let Some(f) = v.as_f64() {
        if f >= 0.0 {
            return Some(f as usize);
        }
    }
    if let Some(s) = v.as_str() {
        if let Ok(f) = s.trim().parse::<f64>() {
            if f >= 0.0 {
                return Some(f as usize);
            }
        }
    }
    None
}

/// `semanticBoolean`-ish coercion: accept JSON bool or boolean-ish string.
fn value_as_bool(v: &Value) -> Option<bool> {
    if let Some(b) = v.as_bool() {
        return Some(b);
    }
    if let Some(s) = v.as_str() {
        match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => return Some(true),
            "false" | "0" | "no" | "off" => return Some(false),
            _ => {}
        }
    }
    None
}

/// Custom `grep_searcher` sink collecting match (and, for content mode, context)
/// lines. Counting is DECOUPLED from recording: `match_count` always counts
/// every match (so count mode reports the true total and content mode is bounded
/// only by `head_limit`, matching TS / `rg`); recording stops past
/// `GREP_RECORDS_CAP` as a pure memory valve, but the search keeps running so the
/// count stays accurate.
struct GrepSink {
    /// `(line_number, text, is_context)` records in encounter order (content
    /// mode only). `is_context` separates `rg`'s context lines from its
    /// matching lines: only matching lines are subject to `-o`, and the two
    /// carry different `--max-columns` markers (ST-13/ST-14).
    records: Vec<(Option<u64>, String, bool)>,
    /// True count of *matched* (not context) lines seen — never capped.
    match_count: usize,
    /// Set when the records valve (`GREP_RECORDS_CAP`) stopped *recording*
    /// lines for this file (counting continued). Surfaces `truncated: true`.
    overflow: bool,
    /// Push match + context lines into `records` (content mode).
    record_lines: bool,
    /// Stop after the first match (`files_with_matches`, mirrors `rg -l`).
    first_match_only: bool,
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // Always count first — counting is never limited (TS / `rg` have no
        // per-file cap; count mode must report the true total).
        self.match_count += 1;

        // `files_with_matches`: one match is enough; stop early like `rg -l`.
        if self.first_match_only {
            return Ok(false);
        }

        // Record lines only while under the memory valve. Past the valve we KEEP
        // SEARCHING (return Ok(true)) so the count keeps climbing — we just stop
        // appending to `records`.
        if self.record_lines && self.records.len() < GREP_RECORDS_CAP {
            self.records
                .push((mat.line_number(), decode_line(mat.bytes()), false));
            if self.records.len() >= GREP_RECORDS_CAP {
                self.overflow = true;
            }
        }
        Ok(true)
    }

    fn context(&mut self, _s: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        // Context lines also respect the records valve (they share the buffer).
        if self.record_lines && self.records.len() < GREP_RECORDS_CAP {
            self.records
                .push((ctx.line_number(), decode_line(ctx.bytes()), true));
        }
        Ok(true)
    }
}

/// `GrepTool` — content search.
pub struct GrepTool {
    ctx: BuiltinToolContext,
    /// Optional shared live-cwd cell (claude-code `getCwd()`/`Ct()`). When
    /// injected (desktop), the no-`path` default dir, the "Path does not exist"
    /// cwd note, and result relativization follow the post-`cd` directory; when
    /// absent (mobile/tests) they fall back to `ctx.workspace`.
    live_cwd: Option<tool_api::LiveCwdCell>,
}

impl GrepTool {
    /// Construct a new tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            live_cwd: None,
        }
    }

    /// Inject the shared live-cwd cell (builder; default is `None`). The desktop
    /// composition root passes the SAME cell the `BashTool` writes on a `cd`.
    #[must_use]
    pub fn with_live_cwd(mut self, cell: tool_api::LiveCwdCell) -> Self {
        self.live_cwd = Some(cell);
        self
    }

    /// The effective live cwd: the injected cell's value if present (OURS
    /// P2-08 — tracks a foreground Bash `cd`), else the session cwd
    /// [`BuiltinToolContext::cwd`] (THEIRS worktree-206 — the boot cwd until an
    /// `EnterWorktree`/`ExitWorktree` swaps `session_cwd`). Both are the
    /// claude-code `Ct()` fallback; they coincide at boot with no cell injected.
    fn cwd_now(&self) -> std::path::PathBuf {
        self.live_cwd
            .as_ref()
            .map(|c| c.lock().unwrap().clone())
            .unwrap_or_else(|| self.ctx.cwd())
    }
}

/// ST-08: property INSERTION order is the wire order (`serde_json` is built
/// with `preserve_order`), and it must be the oracle's `Dhv` order:
/// `pattern, path, glob, output_mode, -B, -A, -C, context, -n, -i, -o, type,
/// head_limit, offset, multiline` (2.1.238 @289931400+). The port previously
/// hoisted `type` to 4th and emitted `-A` before `-B`.
///
/// Also ST-08: NO `default` keys. Every optional field upstream is
/// `ece(Xe().optional())` / `xq(Bt().optional())` — a `z.preprocess` around a
/// bare `.optional()` with no `.default()`, which zod-to-json-schema renders
/// WITHOUT a `default`. (Contrast Edit's `replace_all: xq(Bt().default(!1)
/// .optional())`, which does carry one — and which `edit.rs` correctly emits.)
/// The six the port used to inject (`output_mode`, `-n`, `-i`, `-o`, `offset`,
/// `multiline`) were model-visible bytes the oracle never sends; the runtime
/// defaults themselves are unaffected — they live in `call()`.
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["pattern"],
        "properties": {
            "pattern":     { "type": "string", "description": "The regular expression pattern to search for in file contents" },
            "path":        { "type": "string", "description": "File or directory to search in (rg PATH). Defaults to current working directory." },
            "glob":        { "type": "string", "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob" },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_with_matches", "count"],
                "description": "Output mode: \"content\" shows matching lines (supports -A/-B/-C context, -n line numbers, head_limit), \"files_with_matches\" shows file paths (supports head_limit), \"count\" shows match counts (supports head_limit). Defaults to \"files_with_matches\"."
            },
            "-B":          { "type": "number", "description": "Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise." },
            "-A":          { "type": "number", "description": "Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise." },
            "-C":          { "type": "number", "description": "Alias for context." },
            "context":     { "type": "number", "description": "Number of lines to show before and after each match (rg -C). Requires output_mode: \"content\", ignored otherwise." },
            "-n":          { "type": "boolean", "description": "Show line numbers in output (rg -n). Requires output_mode: \"content\", ignored otherwise. Defaults to true." },
            "-i":          { "type": "boolean", "description": "Case insensitive search (rg -i)" },
            "-o":          { "type": "boolean", "description": "Print only the matched (non-empty) parts of each matching line, one match per output line (rg -o / --only-matching). Requires output_mode: \"content\", ignored otherwise. Defaults to false." },
            "type":        { "type": "string", "description": "File type to search (rg --type). Common types: js, py, rust, go, java, etc. More efficient than include for standard file types." },
            "head_limit":  { "type": "number", "description": "Limit output to first N lines/entries, equivalent to \"| head -N\". Works across all output modes: content (limits output lines), files_with_matches (limits file paths), count (limits count entries). Defaults to 250 when unspecified. Pass 0 for unlimited (use sparingly — large result sets waste context)." },
            "offset":      { "type": "number", "description": "Skip first N lines/entries before applying head_limit, equivalent to \"| tail -n +N | head -N\". Works across all output modes. Defaults to 0." },
            "multiline":   { "type": "boolean", "description": "Enable multiline mode where . matches newlines and patterns can span lines (rg -U --multiline-dotall). Default: false." }
        }
    })
});

static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type":"object",
        "required":["numFiles","filenames"],
        "properties":{
            "mode":{"type":"string","enum":["content","files_with_matches","count"]},
            "numFiles":{"type":"number"},
            "filenames":{"type":"array","items":{"type":"string"}},
            "content":{"type":"string"},
            "numLines":{"type":"number"},
            "numMatches":{"type":"number"},
            "totalFiles":{"type":"number"},
            "totalLines":{"type":"number"},
            "appliedLimit":{"type":"number"},
            "appliedOffset":{"type":"number"}
        }
    })
});

struct GrepWalkArgs {
    canon_base: PathBuf,
    search_resolution: SearchResolutionSnapshot,
    overrides: ignore::overrides::Override,
    types_filter: Option<ignore::types::Types>,
    matcher: RegexMatcher,
    cwd_for_rel: PathBuf,
    deadline: Instant,
    content_mode: bool,
    files_mode: bool,
    count_mode: bool,
    only_matching: bool,
    show_line_numbers: bool,
    multiline: bool,
    ctx_before: Option<usize>,
    ctx_after: Option<usize>,
}

struct GrepWalkResult {
    content_lines: Vec<String>,
    count_lines: Vec<String>,
    files_matched: Vec<(PathBuf, SystemTime)>,
    total_matches: u64,
    timed_out: bool,
}

fn grep_walk(args: GrepWalkArgs) -> Result<GrepWalkResult, SearchResolutionError> {
    let GrepWalkArgs {
        canon_base,
        search_resolution,
        overrides,
        types_filter,
        matcher,
        cwd_for_rel,
        deadline,
        content_mode,
        files_mode,
        count_mode,
        only_matching,
        show_line_numbers,
        multiline,
        ctx_before,
        ctx_after,
    } = args;

    search_resolution.verify()?;
    let mut wb = WalkBuilder::new(&canon_base);
    wb.hidden(false); // --hidden: search dotfiles
    wb.overrides(overrides);
    if let Some(t) = types_filter {
        wb.types(t);
    }

    let mut content_lines: Vec<String> = Vec::new();
    let mut count_lines: Vec<String> = Vec::new();
    let mut files_matched: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut total_matches: u64 = 0;
    let mut timed_out = false;

    for entry in wb.build() {
        search_resolution.verify()?;
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();

        search_resolution.verify()?;
        // The walker's regular-file classification is pathname based. Pin the
        // candidate through a rooted no-follow handle after the classification
        // (with a deterministic test seam in between), then feed that handle
        // directly to the searcher so a leaf/ancestor swap cannot reopen an
        // external pathname.
        run_search_candidate_hook(path);
        let mut file = open_rooted_search_file(&canon_base, path)?;

        let mut sb = SearcherBuilder::new();
        sb.multi_line(multiline);
        sb.line_number(content_mode && show_line_numbers);
        if let Some(b) = ctx_before {
            sb.before_context(b);
        }
        if let Some(a) = ctx_after {
            sb.after_context(a);
        }
        let mut searcher = sb.build();

        let mut sink = GrepSink {
            records: Vec::new(),
            match_count: 0,
            overflow: false,
            record_lines: content_mode,
            first_match_only: files_mode,
        };
        if searcher
            .search_reader(&matcher, &mut file, &mut sink)
            .is_err()
        {
            return Err(SearchResolutionError::SearchRootChanged);
        }

        total_matches += sink.match_count as u64;
        if sink.match_count == 0 {
            continue;
        }

        if content_mode {
            let rel = to_relative_path(path, &cwd_for_rel);
            for (lnum, text, is_context) in &sink.records {
                // ST-14: `-o` applies to MATCHING lines only. The oracle
                // pushes `-o` and the `-C/-B/-A` block independently
                // (`if(d&&o==="content")_.push("-o")` … `if(o==="content"){…}`,
                // @289928728), and ripgrep prints context lines in full
                // under `-o` — verified:
                //   `rg -n -o -C 1 'MATCH[0-9]?'` → `2-bbb`, `3:MATCH`,
                //   `3:MATCH2`, `4-ccc`.
                if only_matching && !*is_context {
                    // rg -o: one matched substring per output line (a line
                    // with multiple matches yields multiple output lines).
                    // ST-13: `--max-columns` measures the EMITTED match here,
                    // not its source line.
                    for m in only_matching_spans(&matcher, text) {
                        let m = apply_max_columns(m, false);
                        let line = match (show_line_numbers, lnum) {
                            (true, Some(n)) => format!("{rel}:{n}:{m}"),
                            _ => format!("{rel}:{m}"),
                        };
                        content_lines.push(line);
                    }
                } else {
                    let text = apply_max_columns(text.clone(), *is_context);
                    let line = match (show_line_numbers, lnum) {
                        (true, Some(n)) => format!("{rel}:{n}:{text}"),
                        _ => format!("{rel}:{text}"),
                    };
                    content_lines.push(line);
                }
            }
        } else if count_mode {
            let rel = to_relative_path(path, &cwd_for_rel);
            count_lines.push(format!("{rel}:{}", sink.match_count));
        } else {
            // files_with_matches: defer relativize until after sort.
            let mtime = file
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            files_matched.push((path.to_path_buf(), mtime));
        }
    }

    search_resolution.verify()?;
    Ok(GrepWalkResult {
        content_lines,
        count_lines,
        files_matched,
        total_matches,
        timed_out,
    })
}

fn search_resolution_error(path: &Path, error: SearchResolutionError) -> ToolError {
    let reason = match error {
        SearchResolutionError::SearchRootChanged => {
            "its symlink resolution changed after permission was checked"
        }
        SearchResolutionError::ReadDenyPathChanged => {
            "a path one of its Read deny rules is written through changed while the search was being prepared. Retry."
        }
    };
    ToolError::InvalidInput(format!("Refusing to search {}: {reason}", path.display()))
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<&str> {
        Some("search file contents with regex (ripgrep)")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }
    fn map_result_text(&self, result: &Value) -> Option<String> {
        map_grep_result_text(result)
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    /// ST-10: Grep carries its OWN cap. Oracle 2.1.238 `Uve = es({name:Am,
    /// searchHint:"search file contents with regex (ripgrep)",
    /// maxResultSizeChars:20000, …})` at binary @289933813 — the port was
    /// returning the generic 30_000, so Grep results were truncated 1.5x later
    /// than upstream. Glob's cap is 100_000 (see `glob.rs`); they differ.
    fn max_result_size_chars(&self) -> usize {
        GREP_MAX_RESULT_SIZE_CHARS
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    /// 1:1 with claude-code Grep `validateInput({path})` (oracle `Uve`,
    /// cc-238.js @226429938). ST-04/ST-05: this is NOT Glob's validator — Grep
    /// accepts a FILE path (`rg PATH`; there is no `isDirectory()` arm) and its
    /// only rejection is ENOENT, worded `Path does not exist: …` rather than
    /// Glob's `Directory does not exist: …` (see [`crate::dir_validate`]).
    /// The cwd (`er()`) is the tool's live workspace.
    ///
    /// The oracle runs three guards, in this order:
    /// 1. ST-06 `h0i(Am,[["pattern",e],["path",t],["glob",r],["type",n]])` — a
    ///    NUL in any of the FOUR string fields (Glob only checks two);
    /// 2. ST-07 the `head_limit`/`offset` whole-number loop;
    /// 3. the ENOENT `stat` on `path`.
    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        crate::dir_validate::validate_no_null_bytes(
            TOOL_NAME,
            &[
                ("pattern", input.get("pattern").and_then(Value::as_str)),
                ("path", input.get("path").and_then(Value::as_str)),
                ("glob", input.get("glob").and_then(Value::as_str)),
                ("type", input.get("type").and_then(Value::as_str)),
            ],
        )?;
        validate_whole_number("head_limit", input.get("head_limit"))?;
        validate_whole_number("offset", input.get("offset"))?;
        // TL-4 (2.1.260): the DISK probe moved to `call` — see the note on
        // `FileGlobTool::validate_input`. Probing before the permission decision
        // told the model whether a path it may not read exists.
        Ok(())
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    /// Oracle `async description(){return Wka(void 0)}` — `qk(undefined)` is
    /// false, so this is the LONG arm, whose Agent bullet is itself gated on the
    /// subagent steer (ST-03).
    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        grep_description(lingxi_core::host::live_sessions::subagent_steer_is_default())
    }

    async fn prompt(&self, opts: &PromptOptions) -> String {
        // Model-gated, mirroring claude-code `prompt({model:e}){return Wka(e)}`
        // where `Wka(e){if(qk(e))return SHORT; return LONG}` (2.1.238 source text
        // @286224735). `description(){return Wka(void 0)}` is hard-pinned to the
        // LONG (`qk(undefined)`=false). Predicate shared with TodoWrite via
        // `tool_api`.
        if tool_api::dh_simple_system_prompt(opts.model.as_deref()) {
            GREP_PROMPT_SHORT.to_string()
        } else {
            grep_description(lingxi_core::host::live_sessions::subagent_steer_is_default())
        }
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // `trusted_dirs` snapshot for this whole call (canonicalize gate). The
        // effective cwd (base fallback + relativize) comes from `cwd_now()`,
        // which prefers the injected live-cwd cell (OURS P2-08, tracks Bash
        // `cd`) and otherwise falls back to `ctx.cwd()` (THEIRS session_cwd).
        let trusted = self.ctx.trusted_dirs();

        // --- Arg parsing (GrepTool.ts:310-326) ---
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("pattern is required".into()))?;
        // claude-code Grep `getPath`: a supplied `path` is used as-is, otherwise
        // the LIVE cwd (`Ct()`). The no-cell fallback (`cwd_now()`==
        // `ctx.workspace`) equals the former `trusted_dirs.first()` boot cwd, so
        // behavior is byte-identical until a Bash `cd` moves the shared cell.
        let base = input
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd_now());
        let glob_filter = input.get("glob").and_then(Value::as_str);
        let type_filter = input.get("type").and_then(Value::as_str);
        let output_mode = input
            .get("output_mode")
            .and_then(Value::as_str)
            .unwrap_or("files_with_matches")
            .to_string();
        let case_insensitive = input.get("-i").and_then(value_as_bool).unwrap_or(false);
        let show_line_numbers = input.get("-n").and_then(value_as_bool).unwrap_or(true);
        let multiline = input
            .get("multiline")
            .and_then(value_as_bool)
            .unwrap_or(false);
        // `-o` / `--only-matching` (rg -o): emit only the matched substrings, one
        // per line. Only meaningful in content mode (claude-code: "Requires
        // output_mode: content") and ignores -A/-B/-C context (like rg -o).
        let only_matching = input.get("-o").and_then(value_as_bool).unwrap_or(false);
        let context_before = input.get("-B").and_then(value_as_usize);
        let context_after = input.get("-A").and_then(value_as_usize);
        let context_c = input.get("-C").and_then(value_as_usize);
        let context_param = input.get("context").and_then(value_as_usize);
        let head_limit = input.get("head_limit").and_then(value_as_usize);
        let offset = input.get("offset").and_then(value_as_usize).unwrap_or(0);

        let content_mode = output_mode == "content";
        let files_mode = output_mode == "files_with_matches";
        let count_mode = output_mode == "count";

        // `USE_BUILTIN_RIPGREP` opt-out surface (binary `A3r`): an explicitly
        // falsy env value + a system `rg` on `$PATH` makes claude-code shell to
        // that binary. LingXi's in-process `grep-*` engine IS the embedded
        // default; resolve + record the mode DECISION here so the env var is
        // consumed faithfully. The system-`rg` SUBPROCESS execution backend is a
        // documented residual (`crate::ripgrep_mode`), so the in-process engine
        // remains the executor for both modes.
        if let crate::ripgrep_mode::RipgrepMode::System { command } =
            crate::ripgrep_mode::resolve_ripgrep_mode()
        {
            tracing::debug!(
                rg = %command.display(),
                "USE_BUILTIN_RIPGREP opt-out selected system rg; in-process engine used (subprocess backend is a residual)"
            );
        }

        let started = Instant::now();

        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let base = match translate_model_path(&self.ctx.fs, base, false) {
            Ok(base) => base,
            Err(message) => return Err(ToolError::InvalidInput(message)),
        };
        // TL-4 (2.1.260): the search-path probe. It lived in `validate_input`,
        // which runs BEFORE the permission decision, so a missing path was
        // reported for a path the session may not be allowed to look at. The
        // oracle's `validateInput` is argument checks only; the probe is `pgs`,
        // called from `call`.
        //
        // It sits AFTER `translate_model_path` on purpose: a mobile guest path
        // must be probed as the host directory that actually backs it. In
        // `validate_input` it ran on the UNTRANSLATED path, so guest Grep failed
        // its own path check on the main dispatch path.
        if input.get("path").and_then(Value::as_str).is_some() {
            crate::dir_validate::validate_grep_path(&base.to_string_lossy(), &self.cwd_now())
                .map_err(|e| ToolError::InvalidInput(e.0))?;
        }
        let canon_base = match canonicalize_and_validate(&base, &trusted) {
            Ok(p) => p,
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &base).await;
                return Err(ToolError::PathBlocked { path: base });
            }
        };

        // Relativize against the (canonicalized) LIVE cwd — mirrors TS
        // `toRelativePath(_, getCwd())`. The walk yields canonicalized paths, so
        // the cwd must be canonicalized too for `strip_prefix` to match.
        let live_cwd = self.cwd_now();
        let cwd_for_rel = std::fs::canonicalize(&live_cwd).unwrap_or_else(|_| live_cwd.clone());

        // --- Build regex matcher (multiline → -U --multiline-dotall) ---
        let matcher = match RegexMatcherBuilder::new()
            .case_insensitive(case_insensitive)
            .multi_line(multiline)
            .dot_matches_new_line(multiline)
            // Keep grep-regex's upstream defaults intact here:
            //   size_limit = 100 MiB
            //   dfa_size_limit = 1000 MiB
            //   nest_limit = 250
            // Claude/ripgrep do not impose LingXi-specific lower caps, and the
            // smaller 10 MiB / 10 MiB / 50 overrides rejected valid patterns.
            .build(pattern)
        {
            Ok(m) => m,
            Err(e) => {
                return Err(ToolError::InvalidInput(format!(
                    "invalid regex {pattern:?}: {e}"
                )));
            }
        };

        // --- Build glob/VCS overrides (GrepTool.ts:332-335, 391-409) ---
        // In OverrideBuilder, `!` is an ignore and a bare glob is a whitelist —
        // exactly `rg --glob` semantics (gitignore-style, matches at any depth).
        let mut ob = OverrideBuilder::new(&canon_base);
        for dir in VCS_DIRECTORIES_TO_EXCLUDE {
            // Both the dir and its contents; pruning handles the descent.
            let _ = ob.add(&format!("!{dir}"));
            let _ = ob.add(&format!("!{dir}/**"));
        }
        // Read(deny) exclusions (GrepTool.ts:417-427): each active `Read`-`deny`
        // rule (resolved by `permission::read_deny_exclude_globs` from the live
        // policy on every call) is turned into a negated override so a
        // denied/sensitive path never appears in results. Prefix EXACTLY as
        // the reference does: a rooted
        // (`/`-anchored) entry → `!P`; a bare relative entry → `!**/P` (match at
        // any depth). In `OverrideBuilder`, a `!`-prefixed pattern is an ignore.
        let deny_globs = self.ctx.effective_read_deny_exclude_globs(&canon_base);
        let search_resolution = SearchResolutionSnapshot::capture(&base, &canon_base, &deny_globs);
        for p in &deny_globs {
            let neg = if p.starts_with('/') {
                format!("!{p}")
            } else {
                format!("!**/{p}")
            };
            let _ = ob.add(&neg);
        }
        if let Some(g) = glob_filter {
            for pat in split_glob_patterns(g) {
                if let Err(e) = ob.add(&pat) {
                    return Err(ToolError::InvalidInput(format!(
                        "invalid glob {pat:?}: {e}"
                    )));
                }
            }
        }
        if let Some(registry) = self.ctx.task_registry.as_ref() {
            if let Some(directory) = registry.task_output_directory().await {
                // Single-file Grep is an explicit read, not a directory traversal.
                if !canon_base.is_file() {
                    let exclusions = lingxi_core::host::task_output::search_exclusions(
                        Path::new(&directory),
                        &canon_base,
                    );
                    if exclusions.iter().any(|p| p == "!**") {
                        return Err(ToolError::InvalidInput("Task output files are read individually: Grep (or Read) a specific task's output path rather than the tasks directory.".into()));
                    }
                    for exclusion in exclusions {
                        ob.add(&exclusion)
                            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
                    }
                }
            }
        }
        let overrides = match ob.build() {
            Ok(o) => o,
            Err(e) => {
                return Err(ToolError::InvalidInput(format!("invalid glob: {e}")));
            }
        };

        // --- Build --type filter (GrepTool.ts:386-389) ---
        let types_filter = match type_filter {
            Some(t) => {
                let mut tb = TypesBuilder::new();
                tb.add_defaults();
                tb.select(t);
                match tb.build() {
                    Ok(types) => Some(types),
                    Err(e) => {
                        return Err(ToolError::InvalidInput(format!("invalid type {t:?}: {e}")));
                    }
                }
            }
            None => None,
        };

        // Effective context windows: context > -C > (-B and/or -A). Content only.
        // `-o` still preserves full context lines; it only changes matching lines.
        let (ctx_before, ctx_after) = if content_mode {
            if let Some(c) = context_param {
                (Some(c), Some(c))
            } else if let Some(c) = context_c {
                (Some(c), Some(c))
            } else {
                (context_before, context_after)
            }
        } else {
            (None, None)
        };

        // --- Walk + search (sync CPU/IO; run off the async runtime) ---
        // --- Wall-clock budget on the walk (`utils/ripgrep.ts:130-133`) ---
        // `LINGXI_GLOB_TIMEOUT_SECONDS` overrides; else 20s (60s on WSL).
        // The in-process equivalent of `rg`'s execFile timeout is a deadline
        // checked each walk step (the walk is synchronous CPU/IO work — no tokio
        // timer / extra dep needed). `Platform::as_str()` returns `getPlatform()`
        // spelling ("wsl") — compared by value so this file needs no `sandbox` dep.
        let is_wsl = self.ctx.platform.as_str() == "wsl";
        let deadline = started + ripgrep_timeout(is_wsl);
        let cwd_for_rel_in_worker = cwd_for_rel.clone();
        let search_path = base.clone();
        let search_path_for_hook = base.clone();

        let GrepWalkResult {
            content_lines,
            count_lines,
            mut files_matched,
            total_matches,
            timed_out,
        } = tokio::task::spawn_blocking(move || {
            // This is deliberately after the deny overrides have been built
            // and immediately before the walk is created. The test hook swaps
            // a symlink at this exact preparation boundary.
            run_search_preparation_hook(&search_path_for_hook);
            search_resolution.verify()?;
            grep_walk(GrepWalkArgs {
                canon_base,
                search_resolution,
                overrides,
                types_filter,
                matcher,
                cwd_for_rel: cwd_for_rel_in_worker,
                deadline,
                content_mode,
                files_mode,
                count_mode,
                only_matching,
                show_line_numbers,
                multiline,
                ctx_before,
                ctx_after,
            })
        })
        .await
        .map_err(|e| ToolError::Io(e.to_string()))?
        .map_err(|error| search_resolution_error(&search_path, error))?;

        // Mirror `utils/ripgrep.ts:444-454`: a timeout with NO results is a hard
        // error (so the model knows the search didn't complete rather than
        // assuming "no matches"); a timeout WITH partial results returns them.
        let had_results =
            total_matches > 0 || !files_matched.is_empty() || !content_lines.is_empty();
        if timed_out && !had_results {
            return Err(ToolError::Io(RIPGREP_TIMEOUT_MSG(is_wsl)));
        }

        // --- Assemble per-mode result data (claude-code GrepTool outputSchema,
        // 2.1.287) + model text. `data.content` is the raw search output; the
        // mapper adds empty-result wording, pagination, and count summaries.
        // No `truncated` — not part of the binary result. ---
        let mut data = Map::new();
        let mc_channel: Option<String>;
        let applied_offset = if offset > 0 { Some(offset) } else { None };

        if content_mode {
            // 2.1.212: carry the full pre-pagination line count (`totalLines:A.length`)
            // so that a page past the end (empty page with `appliedOffset` set and
            // `totalLines > 0`) reports "No entries at this offset" instead of the
            // bare "No matches found" — matching `mapToolResultToToolResultBlockParam`'s
            // content branch `m = n || (c && (a ?? 0) > 0 ? "No entries at this offset"
            // : "No matches found")` (c=appliedOffset, a=totalLines).
            let total_lines = content_lines.len();
            let (limited, applied_limit) = apply_head_limit(content_lines, head_limit, offset);
            let num_lines = limited.len();
            let content_str = limited.join("\n");
            let limit_info = format_limit_info(applied_limit, offset);
            let result_content = if content_str.is_empty() {
                if applied_offset.is_some() && total_lines > 0 {
                    "No entries at this offset".to_string()
                } else {
                    "No matches found".to_string()
                }
            } else {
                content_str.clone()
            };
            let model = if limit_info.is_empty() {
                result_content
            } else {
                format!("{result_content}\n\n[Showing results with pagination = {limit_info}]")
            };
            // binary: {mode, numFiles, filenames:[], content, numLines, totalLines, appliedLimit?, appliedOffset?}
            data.insert("mode".to_string(), json!("content"));
            data.insert("numFiles".to_string(), json!(0));
            data.insert("filenames".to_string(), json!([] as [String; 0]));
            data.insert("content".to_string(), json!(content_str));
            data.insert("numLines".to_string(), json!(num_lines));
            data.insert("totalLines".to_string(), json!(total_lines));
            if let Some(l) = applied_limit {
                data.insert("appliedLimit".to_string(), json!(l));
            }
            if let Some(o) = applied_offset {
                data.insert("appliedOffset".to_string(), json!(o));
            }
            mc_channel = Some(model);
        } else if count_mode {
            let total_file_count = u64::try_from(count_lines.len()).unwrap_or(u64::MAX);
            let (limited, applied_limit) = apply_head_limit(count_lines, head_limit, offset);
            let total = total_matches;
            let limit_info = format_limit_info(applied_limit, offset);
            // ST-09: `g = n || (m>0 ? "No entries at this offset" : "No matches
            // found")` (oracle `mapToolResultToToolResultBlockParam`, count
            // branch @289935600) — `m` is `numMatches`, the TOTAL across every
            // file BEFORE pagination. So a page past the end says "No entries at
            // this offset", not "No matches found". The summary line is appended
            // either way (`g+y`).
            let raw_content = if limited.is_empty() {
                if total_matches > 0 {
                    "No entries at this offset".to_string()
                } else {
                    "No matches found".to_string()
                }
            } else {
                limited.join("\n")
            };
            let occ = if total == 1 {
                "occurrence"
            } else {
                "occurrences"
            };
            let fpl = if total_file_count == 1 {
                "file"
            } else {
                "files"
            };
            let pag = if limit_info.is_empty() {
                String::new()
            } else {
                format!(" with pagination = {limit_info}")
            };
            let summary =
                format!("\n\nFound {total} total {occ} across {total_file_count} {fpl}.{pag}");
            let model = format!("{raw_content}{summary}");
            // binary: {mode, numFiles, filenames:[], content, numMatches, appliedLimit?, appliedOffset?}
            data.insert("mode".to_string(), json!("count"));
            data.insert("numFiles".to_string(), json!(total_file_count));
            data.insert("filenames".to_string(), json!([] as [String; 0]));
            data.insert("content".to_string(), json!(limited.join("\n")));
            data.insert("numMatches".to_string(), json!(total));
            if let Some(l) = applied_limit {
                data.insert("appliedLimit".to_string(), json!(l));
            }
            if let Some(o) = applied_offset {
                data.insert("appliedOffset".to_string(), json!(o));
            }
            mc_channel = Some(model);
        } else {
            // files_with_matches (default). Sort mtime-desc + filename tiebreak;
            // pure filename sort under cfg!(test) (TS NODE_ENV === 'test').
            // The equal-mtime tiebreak uses JS `localeCompare` semantics (binary
            // @200995124: `.sort((P,L)=>{let D=L[1]-P[1];if(D===0)return
            // P[0].localeCompare(L[0]);return D})`), NOT Rust byte Ord — see
            // [`locale_compare`].
            files_matched.sort_by(|a, b| {
                if cfg!(test) {
                    locale_compare(&a.0.to_string_lossy(), &b.0.to_string_lossy())
                } else {
                    match b.1.cmp(&a.1) {
                        std::cmp::Ordering::Equal => {
                            locale_compare(&a.0.to_string_lossy(), &b.0.to_string_lossy())
                        }
                        other => other,
                    }
                }
            });
            let sorted: Vec<PathBuf> = files_matched.into_iter().map(|(p, _)| p).collect();
            // ST-09: `totalFiles: A.length` — the match count BEFORE pagination.
            let total_files = sorted.len();
            let (limited, applied_limit) = apply_head_limit(sorted, head_limit, offset);
            let relpaths: Vec<String> = limited
                .iter()
                .map(|p| to_relative_path(p, &cwd_for_rel))
                .collect();
            let num_files = relpaths.len();
            let limit_info = format_limit_info(applied_limit, offset);
            // ST-09: the oracle's files branch is
            // `if(t===0) content = c&&(s??0)>0 ? `No entries at this offset.
            // [Showing results with pagination = ${d}]` : "No files found"`
            // (t=numFiles, c=appliedOffset, s=totalFiles, d=bKa(limit,offset)).
            // Note the pagination note is INLINE after a period here, unlike
            // content mode's `\n\n[Showing …]`.
            let model = if num_files == 0 {
                if applied_offset.is_some() && total_files > 0 {
                    format!(
                        "No entries at this offset. [Showing results with pagination = {limit_info}]"
                    )
                } else {
                    "No files found".to_string()
                }
            } else {
                let suffix = if limit_info.is_empty() {
                    String::new()
                } else {
                    format!(" {limit_info}")
                };
                format!(
                    "Found {num_files} {}{suffix}\n{}",
                    plural(num_files, "file"),
                    relpaths.join("\n")
                )
            };
            // binary: {mode, filenames, numFiles, totalFiles, appliedLimit?,
            // appliedOffset?} — NO content (the filenames ARE the result); model
            // text → channel. `totalFiles` is declared in the oracle's
            // outputSchema (`Hhv`) and drives the "No entries at this offset"
            // page above (ST-09).
            data.insert("mode".to_string(), json!("files_with_matches"));
            data.insert("filenames".to_string(), json!(relpaths));
            data.insert("numFiles".to_string(), json!(num_files));
            data.insert("totalFiles".to_string(), json!(total_files));
            if let Some(l) = applied_limit {
                data.insert("appliedLimit".to_string(), json!(l));
            }
            if let Some(o) = applied_offset {
                data.insert("appliedOffset".to_string(), json!(o));
            }
            mc_channel = Some(model);
        }

        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: Value::Object(data),
            model_content: mc_channel,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tempfile::TempDir;
    use tool_api::test_support::{fresh_ctx, fresh_tx, make_dummy_fs};

    /// S2 (PathAtlas): a guest base directory translates onto its host twin
    /// (untranslated it would fail containment as a nonexistent path).
    #[tokio::test]
    async fn guest_base_dir_translates_onto_the_host_twin() {
        let host = tempfile::TempDir::new().unwrap();
        std::fs::write(host.path().join("hit.txt"), "needle here").unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_guest_alias_fs("/workspace/abc", host.path(), "/fenced"),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![host.path().to_path_buf()],
        );
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                serde_json::json!({ "pattern": "needle", "path": "/workspace/abc" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(
            result.data.to_string().contains("hit.txt"),
            "grep over a guest base must search host files: {}",
            result.data
        );
    }

    #[test]
    fn locale_compare_matches_js_localecompare() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        // Cases probed against node `String.prototype.localeCompare` (en-US):
        assert_eq!(locale_compare("a", "B"), Less); // case-insensitive primary (byte Ord would be Greater)
        assert_eq!(locale_compare("B", "c"), Less);
        assert_eq!(locale_compare("a", "A"), Less); // lowercase before uppercase
        assert_eq!(locale_compare("A", "a"), Greater);
        assert_eq!(locale_compare("abc", "Abc"), Less);
        assert_eq!(locale_compare("README", "readme"), Greater);
        assert_eq!(locale_compare("_x", "ax"), Less); // '_' before 'a'
        assert_eq!(locale_compare("file2", "file10"), Greater); // NOT numeric: '2' > '1'
        assert_eq!(locale_compare("Z", "a"), Greater);
        assert_eq!(locale_compare("a.txt", "A.txt"), Less);
        assert_eq!(locale_compare("same", "same"), Equal);
    }

    pub(crate) fn make_ctx(tmp: &TempDir) -> (BuiltinToolContext, Arc<InMemorySink>) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        (
            tool_api::test_support::ctx_for_file_tools(
                make_dummy_fs(),
                bus,
                vec![tmp.path().to_path_buf()],
            ),
            sink,
        )
    }

    fn content_str(result: &ToolCallResult) -> String {
        // The structured content/count payload is raw; all three modes carry
        // their model-facing render on `model_content`.
        result
            .model_content
            .as_deref()
            .or_else(|| result.data.get("content").and_then(Value::as_str))
            .expect("content in data or model_content channel")
            .to_string()
    }

    #[test]
    fn mod_replacement_uses_greps_result_mapper() {
        assert!(tool_api::output_schema::validate(
            &OUTPUT_SCHEMA,
            &json!({"numFiles":0,"filenames":[],"mode":"content","content":""})
        )
        .is_ok());
        assert!(tool_api::output_schema::validate(
            &OUTPUT_SCHEMA,
            &json!({"numFiles":0,"filenames":[],"mode":"invalid"})
        )
        .is_err());
        let content = json!({
            "mode":"content", "content":"a.rs:1:match", "appliedLimit":3,
            "numFiles":0, "filenames":[]
        });
        assert_eq!(
            map_grep_result_text(&content).as_deref(),
            Some("a.rs:1:match\n\n[Showing results with pagination = limit: 3]")
        );
        let count = json!({
            "mode":"count", "content":"", "numMatches":2,
            "numFiles":1, "filenames":[], "appliedOffset":5
        });
        assert_eq!(
            map_grep_result_text(&count).as_deref(),
            Some("No entries at this offset\n\nFound 2 total occurrences across 1 file. with pagination = offset: 5")
        );
        let files = json!({
            "mode":"files_with_matches", "numFiles":2,
            "filenames":["a.rs","b.rs"], "appliedOffset":1
        });
        assert_eq!(
            map_grep_result_text(&files).as_deref(),
            Some("Found 2 files offset: 1\na.rs\nb.rs")
        );
        let past_end = json!({
            "mode":"files_with_matches", "numFiles":0, "filenames":[],
            "totalFiles":4, "appliedOffset":10
        });
        assert_eq!(
            map_grep_result_text(&past_end).as_deref(),
            Some("No entries at this offset. [Showing results with pagination = offset: 10]")
        );
    }

    #[test]
    fn tool_name_is_grep() {
        assert_eq!(TOOL_NAME, "Grep");
    }

    #[tokio::test]
    async fn prompt_is_model_gated() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        // description() = Ajr(void 0) ⇒ always LONG.
        let d = tool
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert_eq!(d, grep_description(true));
        // prompt(model:None) ⇒ Dh(undefined)=false ⇒ LONG.
        let long = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
                ..Default::default()
            })
            .await;
        assert_eq!(long, grep_description(true));
        // prompt(model:claude-opus-4-8) ⇒ Dh=true ⇒ SHORT (byte-anchor).
        let short = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: Some("claude-opus-4-8".to_string()),
                model_profile: None,
                ..Default::default()
            })
            .await;
        assert_eq!(short, GREP_PROMPT_SHORT);
        assert!(short.starts_with(
            "Content search built on ripgrep. Prefer this over `grep`/`rg` via a registered shell tool \u{2014} results integrate with the permission UI and file links."
        ));
        // Regex snippets render with single backslashes.
        assert!(short.contains("\"function\\s+\\w+\""));
        assert!(short.contains("escape literal braces (`interface\\{\\}`)."));
        assert!(short.ends_with("- `multiline: true` for patterns that span lines."));
    }

    /// Worktree parity plan (Task 2) INERT INVARIANT: `BuiltinToolContext`
    /// now carries `session_cwd: Arc<SessionCwd>` instead of frozen
    /// `workspace`/`trusted_dirs` fields. With nothing ever calling
    /// `session_cwd.swap(..)` (no `EnterWorktree` in this test), `ctx.cwd()`
    /// must equal the boot cwd it was constructed with, and `Grep` must
    /// return the SAME result it did before the migration.
    #[tokio::test]
    async fn no_swap_is_identical() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);

        // No `session_cwd.swap(..)` call anywhere in this test — `cwd()` must
        // still read back exactly the boot value `make_ctx` constructed.
        assert_eq!(ctx.cwd(), tmp.path());
        assert_eq!(ctx.trusted_dirs(), vec![tmp.path().to_path_buf()]);

        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "count" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["numMatches"], 1);
        assert_eq!(result.data["numFiles"], 1);
    }

    /// Count mode must report the TRUE per-file + total count, with NO per-file
    /// cap (matching TS / `rg`). A file with 150 matches reports 150, not 100.
    #[tokio::test]
    async fn count_mode_reports_true_count_no_cap() {
        let tmp = TempDir::new().unwrap();
        let content: String = (0..150).map(|i| format!("fn f{i}() {{}}\n")).collect();
        std::fs::write(tmp.path().join("big.rs"), content).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "count" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // True count (150), not the old GREP_PER_FILE_CAP (100).
        assert_eq!(result.data["numMatches"], 150);
        assert_eq!(result.data["numFiles"], 1);
        let c = content_str(&result);
        assert!(
            c.contains("big.rs:150"),
            "per-file count should be 150: {c}"
        );
        assert!(
            c.ends_with("\n\nFound 150 total occurrences across 1 file."),
            "summary should report 150: {c}"
        );
    }

    /// Content mode is bounded only by `head_limit` (default 250), not a per-file
    /// cap: a 150-match file under unlimited `head_limit` records ALL 150 lines.
    #[tokio::test]
    async fn content_mode_no_per_file_cap_under_unlimited() {
        let tmp = TempDir::new().unwrap();
        let content: String = (0..150).map(|i| format!("fn f{i}() {{}}\n")).collect();
        std::fs::write(tmp.path().join("big.rs"), content).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "content", "head_limit": 0 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // All 150 lines recorded (old cap would have stopped at 100).
        assert_eq!(result.data["numLines"], 150);
        // (`truncated` is not part of the binary GrepTool result.)
    }

    #[test]
    fn apply_head_limit_byte_faithful() {
        // limit 0 = unlimited (offset still applied).
        let (out, applied) = apply_head_limit(vec![1, 2, 3, 4, 5], Some(0), 2);
        assert_eq!(out, vec![3, 4, 5]);
        assert_eq!(applied, None);
        // truncation reports the effective limit.
        let (out, applied) = apply_head_limit(vec![1, 2, 3, 4, 5], Some(2), 0);
        assert_eq!(out, vec![1, 2]);
        assert_eq!(applied, Some(2));
        // no truncation reports None.
        let (out, applied) = apply_head_limit(vec![1, 2], Some(5), 0);
        assert_eq!(out, vec![1, 2]);
        assert_eq!(applied, None);
        // default limit (250) unspecified.
        let (out, applied) = apply_head_limit((0..300).collect::<Vec<_>>(), None, 0);
        assert_eq!(out.len(), 250);
        assert_eq!(applied, Some(250));
    }

    #[test]
    fn format_limit_info_byte_faithful() {
        assert_eq!(format_limit_info(None, 0), "");
        assert_eq!(format_limit_info(Some(3), 0), "limit: 3");
        assert_eq!(format_limit_info(None, 5), "offset: 5");
        assert_eq!(format_limit_info(Some(3), 5), "limit: 3, offset: 5");
    }

    #[test]
    fn is_env_truthy_mirrors_ts() {
        // Unset/empty → falls back to the default's string (|| 'true').
        std::env::remove_var("LX_TEST_TOGGLE");
        assert!(is_env_truthy("LX_TEST_TOGGLE", true));
        assert!(!is_env_truthy("LX_TEST_TOGGLE", false));
        std::env::set_var("LX_TEST_TOGGLE", "");
        assert!(is_env_truthy("LX_TEST_TOGGLE", true));
        // Explicit truthy tokens (lower+trim).
        for v in ["1", "true", "YES", " on "] {
            std::env::set_var("LX_TEST_TOGGLE", v);
            assert!(
                is_env_truthy("LX_TEST_TOGGLE", false),
                "{v} should be truthy"
            );
        }
        // Anything else is falsy (even with default=true, an explicit value wins).
        for v in ["0", "false", "no", "off", "garbage"] {
            std::env::set_var("LX_TEST_TOGGLE", v);
            assert!(
                !is_env_truthy("LX_TEST_TOGGLE", true),
                "{v} should be falsy"
            );
        }
        std::env::remove_var("LX_TEST_TOGGLE");
    }

    #[test]
    fn ripgrep_timeout_defaults_and_override() {
        // The SAME lock `glob.rs` takes: both modules read
        // LINGXI_GLOB_TIMEOUT_SECONDS and share one process environment in one
        // test binary. Without this, glob's helper cleared the var between
        // this test's `set` and its next read, and the assertion saw the 60s
        // WSL default instead of the 5s override.
        let _env = crate::test_env::guard_file_env();
        assert_eq!(ripgrep_timeout(false), Duration::from_secs(20));
        assert_eq!(ripgrep_timeout(true), Duration::from_secs(60)); // WSL
        std::env::set_var("LINGXI_GLOB_TIMEOUT_SECONDS", "5");
        assert_eq!(ripgrep_timeout(false), Duration::from_secs(5));
        assert_eq!(ripgrep_timeout(true), Duration::from_secs(5)); // override wins over WSL
                                                                   // Non-positive / garbage → default.
        std::env::set_var("LINGXI_GLOB_TIMEOUT_SECONDS", "0");
        assert_eq!(ripgrep_timeout(false), Duration::from_secs(20));
        std::env::set_var("LINGXI_GLOB_TIMEOUT_SECONDS", "nope");
        assert_eq!(ripgrep_timeout(false), Duration::from_secs(20));
        // No manual cleanup: the guard restores the prior value on drop.
    }

    #[test]
    fn ripgrep_timeout_msg_verbatim() {
        assert_eq!(
            RIPGREP_TIMEOUT_MSG(false),
            "Ripgrep search timed out after 20 seconds. The search may have matched files but did not complete in time. Try searching a more specific path or pattern."
        );
        assert_eq!(
            RIPGREP_TIMEOUT_MSG(true),
            "Ripgrep search timed out after 60 seconds. The search may have matched files but did not complete in time. Try searching a more specific path or pattern."
        );
    }

    #[test]
    fn split_glob_patterns_mirrors_ts() {
        assert_eq!(split_glob_patterns("*.rs"), vec!["*.rs"]);
        assert_eq!(split_glob_patterns("*.js,*.ts"), vec!["*.js", "*.ts"]);
        assert_eq!(split_glob_patterns("*.{ts,tsx}"), vec!["*.{ts,tsx}"]);
        assert_eq!(split_glob_patterns("*.js *.ts"), vec!["*.js", "*.ts"]);
    }

    #[tokio::test]
    async fn task_output_directory_is_excluded_but_explicit_file_remains_readable() {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("temp[1]/project/session_current/tasks");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(output.join("b12345678.output"), "needle secret\n").unwrap();
        std::fs::write(tmp.path().join("public.txt"), "needle public\n").unwrap();
        let (mut context, _) = make_ctx(&tmp);
        context.task_registry = Some(std::sync::Arc::new(crate::shared::TaskOutputTestRegistry(
            output.clone(),
        )));
        let tool = GrepTool::new(context);
        let result = tool
            .call(
                json!({"pattern":"needle", "glob":"**/*"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["numFiles"], 1);
        assert!(content_str(&result).contains("public.txt"));
        let error = tool
            .call(
                json!({"pattern":"needle","path":output}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Task output files are read individually"));
        let result = tool
            .call(
                json!({"pattern":"needle","path":output.join("b12345678.output")}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["numFiles"], 1);
    }

    #[tokio::test]
    async fn default_mode_files_with_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\nfn bar() {}").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn baz() {}").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "no match here").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(json!({ "pattern": "fn" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        // Filename sort under cfg!(test): a.rs before b.rs.
        assert_eq!(content_str(&result), "Found 2 files\na.rs\nb.rs");
        assert_eq!(result.data["numFiles"], 2);
        assert_eq!(result.data["mode"], "files_with_matches");
    }

    #[tokio::test]
    async fn default_mode_no_files_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "nothing relevant").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(json!({ "pattern": "zzz_nomatch" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert_eq!(content_str(&result), "No files found");
        assert_eq!(result.data["numFiles"], 0);
    }

    #[tokio::test]
    async fn glob_filter_narrows_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "fn bar()").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "glob": "*.rs" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(content_str(&result), "Found 1 file\na.rs");
    }

    /// Read(deny) exclude globs (GrepTool.ts:417-427): a populated
    /// `read_deny_exclude_globs` removes denied paths from the search. A rooted
    /// `/secrets/**` entry (`!P` prefixing) and a bare relative `.env` entry
    /// (`!**/P` prefixing) are both honored; an empty set is a no-op.
    #[tokio::test]
    async fn read_deny_exclude_globs_skip_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}").unwrap();
        std::fs::write(tmp.path().join(".env"), "fn secret() {}").unwrap();
        let secrets = tmp.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        std::fs::write(secrets.join("key.rs"), "fn bar() {}").unwrap();

        // Baseline: no excludes → all three files match.
        {
            let (ctx, _sink) = make_ctx(&tmp);
            let tool = GrepTool::new(ctx);
            let out = content_str(
                &tool
                    .call(json!({ "pattern": "fn" }), fresh_ctx(), fresh_tx())
                    .await
                    .unwrap(),
            );
            assert!(out.contains("a.rs"), "baseline: {out}");
            assert!(out.contains("secrets/key.rs"), "baseline: {out}");
            assert!(out.contains(".env"), "baseline: {out}");
        }

        // Populated: rooted `/secrets/**` (→ `!/secrets/**`) prunes the dir, and
        // bare `.env` (→ `!**/.env`) prunes the dotfile; `a.rs` survives.
        {
            let (mut ctx, _sink) = make_ctx(&tmp);
            ctx.read_deny_exclude_globs = vec!["/secrets/**".to_string(), ".env".to_string()];
            let tool = GrepTool::new(ctx);
            let out = content_str(
                &tool
                    .call(json!({ "pattern": "fn" }), fresh_ctx(), fresh_tx())
                    .await
                    .unwrap(),
            );
            assert!(out.contains("a.rs"), "non-denied survives: {out}");
            assert!(
                !out.contains("secrets/key.rs"),
                "rooted deny pruned secrets/: {out}"
            );
            assert!(!out.contains(".env"), "relative deny pruned .env: {out}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stable_symlink_search_root_is_allowed() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("hit.rs"), "needle\n").unwrap();
        let link = tmp.path().join("search-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "needle", "path": link }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let matches = result.data["filenames"].as_array().unwrap();
        assert!(matches
            .iter()
            .any(|value| value.as_str().unwrap().ends_with("target/hit.rs")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn candidate_leaf_swap_is_refused_before_grep_reads_content() {
        let tmp = TempDir::new().unwrap();
        let victim = TempDir::new().unwrap();
        let candidate = std::fs::canonicalize(tmp.path())
            .unwrap()
            .join("candidate.rs");
        std::fs::write(&candidate, "needle approved\n").unwrap();
        let victim_file = victim.path().join("victim.rs");
        std::fs::write(&victim_file, "needle SECRET_VICTIM_CONTENT\n").unwrap();

        crate::shared::install_search_candidate_hook(&candidate, {
            let candidate = candidate.clone();
            let victim_file = victim_file.clone();
            move || {
                std::fs::remove_file(&candidate).unwrap();
                std::os::unix::fs::symlink(victim_file, candidate).unwrap();
            }
        });

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let error = tool
            .call(
                json!({
                    "pattern": "needle",
                    "path": tmp.path(),
                    "output_mode": "content"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink resolution changed"));
        assert!(!error.to_string().contains("SECRET_VICTIM_CONTENT"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_root_symlink_retarget_is_refused_before_walk() {
        let tmp = TempDir::new().unwrap();
        let approved = tmp.path().join("approved");
        let victim = tmp.path().join("victim");
        std::fs::create_dir(&approved).unwrap();
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(approved.join("approved.rs"), "approved\n").unwrap();
        std::fs::write(victim.join("victim.rs"), "victim\n").unwrap();
        let link = tmp.path().join("search-link");
        std::os::unix::fs::symlink(&approved, &link).unwrap();

        crate::shared::install_search_preparation_hook(&link, {
            let link = link.clone();
            let victim = victim.clone();
            move || {
                std::fs::remove_file(&link).unwrap();
                std::os::unix::fs::symlink(victim, link).unwrap();
            }
        });

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let error = tool
            .call(
                json!({ "pattern": "needle", "path": link }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "invalid input: Refusing to search {}: its symlink resolution changed after permission was checked",
                tmp.path().join("search-link").display()
            )
        );
        assert!(std::fs::read_to_string(victim.join("victim.rs"))
            .unwrap()
            .contains("victim"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_deny_symlink_retarget_is_refused_before_walk() {
        let tmp = TempDir::new().unwrap();
        let approved = tmp.path().join("approved-deny");
        let victim = tmp.path().join("victim-deny");
        std::fs::create_dir(&approved).unwrap();
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("victim.rs"), "victim\n").unwrap();
        let link = tmp.path().join("deny-link");
        std::os::unix::fs::symlink(&approved, &link).unwrap();

        crate::shared::install_search_preparation_hook(tmp.path(), {
            let link = link.clone();
            let victim = victim.clone();
            move || {
                std::fs::remove_file(&link).unwrap();
                std::os::unix::fs::symlink(victim, link).unwrap();
            }
        });

        let (mut ctx, _sink) = make_ctx(&tmp);
        ctx.read_deny_exclude_globs = vec!["/deny-link/**".to_string()];
        let tool = GrepTool::new(ctx);
        let error = tool
            .call(json!({ "pattern": "victim" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "invalid input: Refusing to search {}: a path one of its Read deny rules is written through changed while the search was being prepared. Retry.",
                tmp.path().display()
            )
        );
        assert!(std::fs::read_to_string(victim.join("victim.rs"))
            .unwrap()
            .contains("victim"));
    }

    #[tokio::test]
    async fn type_filter_rust_only() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "fn bar()").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "type": "rust" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(content_str(&result), "Found 1 file\na.rs");
    }

    #[tokio::test]
    async fn content_mode_line_numbers() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": r"fn \w+", "output_mode": "content" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // -n defaults true: relpath:line:text.
        assert_eq!(
            content_str(&result),
            "a.rs:1:fn foo() {}\na.rs:2:fn bar() {}"
        );
        assert_eq!(result.data["numLines"], 2);
    }

    #[tokio::test]
    async fn content_mode_no_line_numbers() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "content", "-n": false }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(content_str(&result), "a.rs:fn foo() {}");
    }

    #[tokio::test]
    async fn content_mode_only_matching() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": r"fn \w+", "output_mode": "content", "-o": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // -o (rg --only-matching): only the matched substrings, with line numbers
        // (-n defaults true). `fn \w+` matches "fn foo" / "fn bar", not the rest.
        assert_eq!(content_str(&result), "a.rs:1:fn foo\na.rs:2:fn bar");
    }

    #[tokio::test]
    async fn content_mode_only_matching_multiple_per_line() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "ab ab ab\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "ab", "output_mode": "content", "-o": true, "-n": false }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Three matches on one line -> three output lines.
        assert_eq!(content_str(&result), "a.txt:ab\na.txt:ab\na.txt:ab");
    }

    #[tokio::test]
    async fn content_mode_context_window() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "line1\nmatch here\nline3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "match", "output_mode": "content", "-B": 1, "-A": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(c.contains("a.txt:1:line1"), "before context: {c}");
        assert!(c.contains("a.txt:2:match here"), "match line: {c}");
        assert!(c.contains("a.txt:3:line3"), "after context: {c}");
    }

    #[tokio::test]
    async fn content_mode_context_c_alias() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "l1\nl2\nhit\nl4\nl5\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "hit", "output_mode": "content", "-C": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(content_str(&result), "a.txt:2:l2\na.txt:3:hit\na.txt:4:l4");
    }

    #[tokio::test]
    async fn content_mode_only_matching_keeps_context_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "line1\nmatch here\nline3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);

        let before_after = tool
            .call(
                json!({
                    "pattern": "match",
                    "output_mode": "content",
                    "-o": true,
                    "-B": 1,
                    "-A": 1
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            content_str(&before_after),
            "a.txt:1:line1\na.txt:2:match\na.txt:3:line3"
        );

        let context_param = tool
            .call(
                json!({
                    "pattern": "match",
                    "output_mode": "content",
                    "-o": true,
                    "context": 1
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            content_str(&context_param),
            "a.txt:1:line1\na.txt:2:match\na.txt:3:line3"
        );
    }

    #[tokio::test]
    async fn content_mode_head_limit_pagination() {
        let tmp = TempDir::new().unwrap();
        let body: String = (0..10).map(|i| format!("fn f{i}\n")).collect();
        std::fs::write(tmp.path().join("a.rs"), body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "content", "head_limit": 3 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(
            c.ends_with("\n\n[Showing results with pagination = limit: 3]"),
            "pagination note: {c}"
        );
        assert_eq!(result.data["numLines"], 3);
        assert_eq!(result.data["appliedLimit"], 3);
        assert!(!result.data["content"]
            .as_str()
            .unwrap()
            .contains("[Showing results"));
        assert_eq!(
            map_grep_result_text(&result.data).as_deref(),
            Some(c.as_str())
        );
    }

    #[tokio::test]
    async fn content_mode_offset_only_pagination() {
        let tmp = TempDir::new().unwrap();
        let body: String = (0..5).map(|i| format!("fn f{i}\n")).collect();
        std::fs::write(tmp.path().join("a.rs"), body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "content", "head_limit": 0, "offset": 2 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(
            c.ends_with("\n\n[Showing results with pagination = offset: 2]"),
            "offset note: {c}"
        );
        // limit 0 = unlimited, so no applied_limit even though offset applied.
        assert!(result.data.get("applied_limit").is_none());
        assert_eq!(result.data["appliedOffset"], 2);
        // 2.1.212: the full pre-pagination line count rides on `totalLines`.
        assert_eq!(result.data["totalLines"], 5);
    }

    /// 2.1.212: paging PAST the last match (empty page, offset>0, totalLines>0)
    /// reports "No entries at this offset" instead of the bare "No matches found".
    #[tokio::test]
    async fn content_mode_offset_past_end_reports_no_entries() {
        let tmp = TempDir::new().unwrap();
        let body: String = (0..3).map(|i| format!("fn f{i}\n")).collect();
        std::fs::write(tmp.path().join("a.rs"), body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "content", "head_limit": 0, "offset": 10 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(
            c.starts_with("No entries at this offset"),
            "paged-past-end should report no entries: {c}"
        );
        assert!(!c.contains("No matches found"), "must not fall back: {c}");
        assert!(
            c.ends_with("\n\n[Showing results with pagination = offset: 10]"),
            "offset note preserved: {c}"
        );
        assert_eq!(result.data["numLines"], 0);
        assert_eq!(result.data["totalLines"], 3);
        assert_eq!(result.data["appliedOffset"], 10);
    }

    /// A genuinely empty search (no offset) still reports "No matches found":
    /// the "No entries at this offset" branch is gated on `appliedOffset` being
    /// set AND `totalLines > 0`.
    #[tokio::test]
    async fn content_mode_no_matches_without_offset() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "nonexistent_zzz", "output_mode": "content" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert_eq!(c, "No matches found");
        assert_eq!(result.data["totalLines"], 0);
    }

    #[tokio::test]
    async fn content_mode_max_columns_truncation() {
        let tmp = TempDir::new().unwrap();
        let long = "a".repeat(600);
        std::fs::write(tmp.path().join("a.txt"), format!("{long}\n")).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "a+", "output_mode": "content", "-n": false }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        // ST-13: ripgrep's `--max-columns 500` does NOT truncate the line — it
        // REPLACES it with an omission marker. This test used to assert the
        // port's invented "slice to the first 500 chars" behaviour, i.e. it was
        // pinning text ripgrep never prints.
        // Content mode prefixes the filename, so the whole rendered line is
        // `a.txt:` + the marker — the marker REPLACES the line's text.
        assert_eq!(c, "a.txt:[Omitted long matching line]", "got:\n{c}");
        assert!(
            !c.contains("aaaa"),
            "the line content must not survive at all; got:\n{c}"
        );
    }

    #[tokio::test]
    async fn count_mode_summary_plural() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a\nfn b\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn c\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "count" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(
            c.ends_with("\n\nFound 3 total occurrences across 2 files."),
            "count summary: {c}"
        );
        assert!(c.contains("a.rs:2"), "per-file count: {c}");
        assert!(c.contains("b.rs:1"), "per-file count: {c}");
        assert_eq!(result.data["numMatches"], 3);
        assert_eq!(result.data["numFiles"], 2);
        assert!(!result.data["content"]
            .as_str()
            .unwrap()
            .contains("Found 3 total occurrences"));
        assert_eq!(
            map_grep_result_text(&result.data).as_deref(),
            Some(c.as_str())
        );
    }

    #[tokio::test]
    async fn count_mode_summary_singular() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn solo\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "count" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(
            c.ends_with("\n\nFound 1 total occurrence across 1 file."),
            "singular summary: {c}"
        );
    }

    #[tokio::test]
    async fn count_mode_pagination_keeps_total_counts() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn b\n").unwrap();
        std::fs::write(tmp.path().join("c.rs"), "fn c\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "count", "head_limit": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let c = content_str(&result);
        assert!(
            c.ends_with("\n\nFound 3 total occurrences across 3 files. with pagination = limit: 1"),
            "count summary should use full totals, not paginated rows: {c}"
        );
        assert_eq!(c.lines().filter(|line| line.ends_with(":1")).count(), 1);
        assert_eq!(result.data["numMatches"], 3);
        assert_eq!(result.data["numFiles"], 3);
        assert_eq!(result.data["appliedLimit"], 1);
    }

    #[tokio::test]
    async fn multiline_matches_across_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.go"), "struct {\n  field int\n}\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        // Without multiline this would not match; with it, dot/[\s\S] span lines.
        let result = tool
            .call(
                json!({ "pattern": r"struct \{[\s\S]*?field", "multiline": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(content_str(&result), "Found 1 file\na.go");
    }

    #[tokio::test]
    async fn accepts_deeply_nested_regex_groups() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("nested.txt"), "x\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        // This valid regex is deeper than the old manual `.nest_limit(50)`.
        // The tool should accept it by preserving grep-regex's upstream
        // defaults instead of reintroducing lower LingXi-specific caps.
        let pattern = format!("{}x{}", "(".repeat(55), ")".repeat(55));
        let result = tool
            .call(
                json!({ "pattern": pattern, "output_mode": "count" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            content_str(&result),
            "nested.txt:1\n\nFound 1 total occurrence across 1 file."
        );
        assert_eq!(result.data["numMatches"], 1);
    }

    #[tokio::test]
    async fn vcs_directories_excluded() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git").join("config"), "fn secret").unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn visible").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(json!({ "pattern": "fn" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        // .git/config must not appear.
        assert_eq!(content_str(&result), "Found 1 file\na.rs");
    }

    /// The records valve (`GREP_RECORDS_CAP`) is a pure memory bound that trips
    /// `truncated` once recording stops — but it NEVER caps the count. A file
    /// with one match over the valve records exactly `GREP_RECORDS_CAP` content
    /// lines yet still reports the true total in count mode.
    #[tokio::test]
    async fn records_valve_sets_truncated_but_not_count() {
        let tmp = TempDir::new().unwrap();
        let n = GREP_RECORDS_CAP + 5;
        let content: String = (0..n).map(|i| format!("fn f{i}() {{}}\n")).collect();
        std::fs::write(tmp.path().join("big.rs"), content).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);

        // Content mode, unlimited head_limit → records valve is the only bound.
        let result = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "content", "head_limit": 0 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // The records valve trips at GREP_RECORDS_CAP — observable as numLines
        // capped (`truncated` is not part of the binary GrepTool result).
        assert_eq!(result.data["numLines"], GREP_RECORDS_CAP as i64);

        // Count mode still reports the TRUE total (valve doesn't cap counting).
        let count = tool
            .call(
                json!({ "pattern": "fn", "output_mode": "count" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(count.data["numMatches"], n as i64);
    }

    #[tokio::test]
    async fn rejects_invalid_regex() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let err = tool
            .call(json!({ "pattern": "(unclosed" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn description_is_byte_faithful() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        let d = tool
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert!(d.starts_with("A powerful search tool built on ripgrep\n"));
        assert!(d.contains("\"files_with_matches\" shows only file paths (default)"));
        assert!(d.contains("use `multiline: true`"));
        assert!(!d.contains("Bash"));
    }

    #[tokio::test]
    async fn search_hint_byte_exact() {
        // 2.1.206 tool-definition searchHint (byte-verified, 2 hits).
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        assert_eq!(
            tool.search_hint(),
            Some("search file contents with regex (ripgrep)")
        );
    }

    // ── P2-08: live-cwd cell drives the no-path default dir + "does not exist" ──

    #[tokio::test]
    async fn live_cwd_cell_drives_default_search_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("root_only.rs"), "needle here").unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("in_sub.rs"), "needle here").unwrap();

        // Cell = tmp/sub (a post-`cd`): a no-path search defaults to the cell, so
        // only the match under sub is returned (relativized against sub).
        let (ctx, _sink) = make_ctx(&tmp);
        let cell = std::sync::Arc::new(std::sync::Mutex::new(sub.clone()));
        let tool = GrepTool::new(ctx).with_live_cwd(cell);
        let names: Vec<String> = tool
            .call(json!({ "pattern": "needle" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap()
            .data["filenames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().replace('\\', "/"))
            .collect();
        assert_eq!(
            names,
            vec!["in_sub.rs".to_string()],
            "cell must drive the default search dir + relativization: {names:?}"
        );
    }

    /// ST-04: the oracle's Grep `validateInput` has no `isDirectory()` arm, so
    /// `Grep(pattern="x", path="src/main.rs")` is legal — it searches that one
    /// file. The port used to route Grep through Glob's directory-only
    /// validator and refuse with `Path is not a directory: …`.
    #[tokio::test]
    async fn validate_input_accepts_a_file_path() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("main.rs");
        std::fs::write(&file, b"needle\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = GrepTool::new(ctx);
        tool.validate_input(
            &json!({ "pattern": "needle", "path": file.to_str().unwrap() }),
            &fresh_ctx(),
        )
        .await
        .expect("a file path must validate for Grep");
    }

    #[tokio::test]
    async fn live_cwd_cell_drives_path_not_found_note() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let cell = std::sync::Arc::new(std::sync::Mutex::new(sub.clone()));
        let tool = GrepTool::new(ctx).with_live_cwd(cell);
        // TL-4: the probe moved from `validate_input` (pre-permission) into
        // `call` (post-permission), so this exercises `call`.
        assert!(
            tool.validate_input(
                &json!({ "pattern": "x", "path": "no_such_dir" }),
                &fresh_ctx()
            )
            .await
            .is_ok(),
            "validate_input must no longer touch disk"
        );
        let err = tool
            .call(
                json!({ "pattern": "x", "path": "no_such_dir" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let err = match err {
            ToolError::InvalidInput(message) => message,
            other => panic!("expected InvalidInput, got {other:?}"),
        };
        // The note prints the LIVE cwd (canonicalized sub), NOT the workspace.
        // ST-05: Grep's lead is `Path does not exist:`, not Glob's
        // `Directory does not exist:`.
        let canon_sub = std::fs::canonicalize(&sub).unwrap();
        assert_eq!(
            err,
            format!(
                "Path does not exist: no_such_dir. Note: your current working directory is {}.",
                canon_sub.display()
            )
        );
    }
}
