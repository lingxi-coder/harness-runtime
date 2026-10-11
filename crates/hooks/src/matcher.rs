//! Hook matcher — byte-faithful port of `matchesPattern` (claude-code
//! `src/utils/hooks.ts:1346-1381`) and the legacy tool-name helpers it relies
//! on (`src/utils/permissions/permissionRuleParser.ts:31-41`).
//!
//! A hook's `matcher` (the `Pre/PostToolUse` tool-name pattern, e.g.
//! `"Write|Edit"`, `"^Bash$"`, `".*"`) gates whether the hook fires for a
//! given tool invocation. [`matches_pattern`] reproduces the three TS branches
//! exactly:
//!
//! 1. `*` / empty → match everything (short-circuit `true`).
//! 2. `^[A-Za-z0-9_|]+$` → a "simple" pattern with no regex metacharacters
//!    other than `|`: an exact match, or — when it contains `|` — a match
//!    against any pipe-separated segment (each `trim()`-med + legacy-normalized).
//! 3. Otherwise → treat the pattern as an anchored-or-not regex. An invalid
//!    regex logs and returns `false` (never panics), exactly like the TS
//!    `try { new RegExp(...) } catch { … return false }`.
//!
//! The `if`-condition (permission-rule content) matcher is ported as
//! [`matches_if_condition`] — a faithful port of `prepareIfConditionMatcher`
//! (`utils/hooks.ts:1390-1422`) composed with each tool's
//! `preparePermissionMatcher` closure. It reuses the permission crate's
//! `PermissionRuleValue::from_rule_string` + per-tool content matchers, and is
//! wired alongside the tool-name matcher in
//! [`crate::registry::HookRegistry::match_event`].

use crate::events::HookEventType;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use permission::shell_command::rule_matches_any_subcommand_with_candidate_units;
use permission::shell_rule_matching::match_wildcard_pattern_utf16;
use permission::PermissionRuleValue;
use rquickjs::{Context, Runtime};
use std::cell::RefCell;
use std::collections::HashMap;

/// the binary's `atf` membership exactly (note: `FileChanged`,
/// `UserPromptSubmit`, `Stop`, the task/teammate events, etc. are NOT in it).
pub(crate) fn comma_mode_for(event_type: &HookEventType) -> bool {
    matches!(
        event_type,
        HookEventType::PreToolUse
            | HookEventType::PostToolUse
            | HookEventType::PostToolUseFailure
            | HookEventType::PermissionRequest
            | HookEventType::PermissionDenied
            | HookEventType::UserPromptExpansion
            | HookEventType::SessionStart
            | HookEventType::SessionEnd
            | HookEventType::Setup
            | HookEventType::PreCompact
            | HookEventType::PostCompact
            | HookEventType::Notification
            | HookEventType::SubagentStart
            | HookEventType::SubagentStop
            | HookEventType::Elicitation
            | HookEventType::ElicitationResult
            | HookEventType::ConfigChange
            | HookEventType::InstructionsLoaded
    )
}

/// Maps a legacy tool name to its canonical name — `normalizeLegacyToolName`
/// (`permissionRuleParser.ts:21-33`): `LEGACY_TOOL_NAME_ALIASES[name] ?? name`.
///
/// The external (non-ant) alias table mirrors the canonical copy in
/// `permission::rule::normalize_legacy_tool_name` (kept in sync; both port the
/// same TS source). The KAIROS-gated `Brief → SendUserMessage` alias is omitted
/// in external builds, matching `permission`.
#[must_use]
pub fn normalize_legacy_tool_name(name: &str) -> String {
    match name {
        "KillShell" => "TaskStop",
        "AgentOutputTool" | "BashOutputTool" => "TaskOutput",
        other => other,
    }
    .to_string()
}

/// Returns the legacy aliases that map to `canonical_name` — the reverse of
/// [`normalize_legacy_tool_name`], mirroring `getLegacyToolNames`
/// (`permissionRuleParser.ts:35-41`): every legacy key whose canonical value
/// equals `canonical_name`, in `LEGACY_TOOL_NAME_ALIASES` insertion order. Used
/// by [`matches_pattern`]'s regex branch for the remaining non-Agent aliases.
#[must_use]
pub fn get_legacy_tool_names(canonical_name: &str) -> Vec<String> {
    match canonical_name {
        "TaskStop" => vec!["KillShell".to_string()],
        "TaskOutput" => vec!["AgentOutputTool".to_string(), "BashOutputTool".to_string()],
        _ => Vec::new(),
    }
}

/// Returns `true` if `matcher` matches `match_query` using the classic
/// (non-comma) matcher mode.
///
/// Thin back-compat wrapper over [`matches_pattern_with`] with
/// `comma_mode = false` and no tool-alias map — exactly claude-code's
/// `matchesPattern(e, t)` (the `n`/`r` parameters defaulting off). Callers that
/// have the event type (and so can decide comma-mode) should use
/// [`matches_pattern_with`] directly; this keeps the simpler call shape for
/// contexts that always want pipe-only matching.
#[must_use]
pub fn matches_pattern(match_query: &str, matcher: &str) -> bool {
    matches_pattern_with(match_query, matcher, false, None)
}

/// Byte-faithful port of `matchesPattern` (`dtf(e,t,n,r)`,
/// `src/utils/hooks.ts`, as of claude-code 2.1.195).
///
/// * `match_query` (`e`) — the value derived from the event (the tool name for
///   tool events; the MCP server name / config source / load reason / changed
///   file basename for the corresponding non-tool events).
/// * `matcher` (`t`) — the hook's declared pattern.
/// * `comma_mode` (`n`) — `true` for events in claude's comma-mode set (`atf`);
///   it widens the "simple pattern" char class to `^[a-zA-Z0-9_|, -]+$` (the
///   space + comma were always allowed in comma-mode; the **`-` hyphen is the
///   v2.1.195 addition**, so hyphenated names — e.g. MCP server names — match as
///   simple list entries) and splits the simple pattern on BOTH `|` and `,`.
///   When `false`, the classic `^[a-zA-Z0-9_|]+$` / `|`-only behavior applies.
/// * `tool_aliases` (`r`) — claude `toolPermissionContext.toolAliases`: an extra
///   runtime alias map layered on top of the static legacy-tool-name map. It
///   expands each matcher segment (`ofn(xP(i), r)`) and the regex-branch reverse
///   lookup (`sfn(e, r)`). The port carries no tool aliases yet, so the registry
///   passes `None`; the map then collapses to a no-op, byte-identical to the TS
///   when `toolAliases` is empty.
#[must_use]
pub fn matches_pattern_with(
    match_query: &str,
    matcher: &str,
    comma_mode: bool,
    tool_aliases: Option<&HashMap<String, String>>,
) -> bool {
    matches_pattern_projection_with(
        &Utf16JsonProjection::from(match_query),
        matcher,
        comma_mode,
        tool_aliases,
    )
}

/// Match a query that may contain isolated UTF-16 units against a hook matcher.
/// Regex patterns run in the crate's JavaScript engine, matching Native's
/// `new RegExp(pattern).test(query)` syntax and string semantics rather than
/// Rust's different regex dialect.
pub(crate) fn matches_pattern_projection_with(
    match_query: &Utf16JsonProjection,
    matcher: &str,
    comma_mode: bool,
    tool_aliases: Option<&HashMap<String, String>>,
) -> bool {
    matches_pattern_source_with(
        match_query,
        &Utf16JsonProjection::from(matcher),
        comma_mode,
        tool_aliases,
    )
}

/// Source-native matcher for a hook query and matcher string.
pub(crate) fn matches_pattern_source_with(
    match_query: &Utf16JsonProjection,
    matcher: &Utf16JsonProjection,
    comma_mode: bool,
    tool_aliases: Option<&HashMap<String, String>>,
) -> bool {
    let Some(query_units) = match_query.string_units("") else {
        return false;
    };
    let Some(matcher_units) = matcher.string_units("") else {
        return false;
    };
    if matcher_units.is_empty() || matcher_units == [b'*' as u16] {
        return true;
    }

    if is_simple_pattern_units(&matcher_units, comma_mode) {
        let mut segment_start = 0;
        for index in 0..=matcher_units.len() {
            let separator = index == matcher_units.len()
                || matcher_units[index] == b'|' as u16
                || (comma_mode && matcher_units[index] == b',' as u16);
            if !separator {
                continue;
            }
            let segment = trim_js_units(&matcher_units[segment_start..index]);
            segment_start = index + 1;
            if segment.is_empty() {
                continue;
            }
            let Ok(segment) = String::from_utf16(segment) else {
                return false;
            };
            let normalized = normalize_legacy_tool_name(&segment);
            if query_units == normalized.encode_utf16().collect::<Vec<_>>() {
                return true;
            }
            if let Some(mapped) = tool_aliases.and_then(|aliases| aliases.get(&normalized)) {
                if mapped != &normalized && query_units == mapped.encode_utf16().collect::<Vec<_>>()
                {
                    return true;
                }
            }
        }
        return false;
    }

    if js_regexp_test(matcher, match_query).unwrap_or_else(|error| {
        tracing::debug!(error = %error, "Invalid regex pattern in hook matcher");
        false
    }) {
        return true;
    }

    if let Some(query_text) = match_query.string_at("") {
        for legacy_name in get_legacy_tool_names(query_text) {
            if js_regexp_test(matcher, &Utf16JsonProjection::from(legacy_name.as_str()))
                .unwrap_or(false)
            {
                return true;
            }
        }
    }

    if let Some(aliases) = tool_aliases {
        for (key, value) in aliases {
            if query_units == value.encode_utf16().collect::<Vec<_>>()
                && js_regexp_test(matcher, &Utf16JsonProjection::from(key.as_str()))
                    .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

pub(crate) fn is_simple_pattern_units(units: &[u16], comma_mode: bool) -> bool {
    !units.is_empty()
        && units.iter().all(|unit| {
            matches!(*unit, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a)
                || *unit == b'_' as u16
                || *unit == b'|' as u16
                || (comma_mode && matches!(*unit, 0x002c | 0x0020 | 0x002d))
        })
}

/// Trim the exact UTF-16 code units removed by JavaScript `String.trim()`.
///
/// Callers that cross into an OS string API should apply this before converting
/// the remaining units to that API's string representation.
pub fn trim_js_units(mut units: &[u16]) -> &[u16] {
    while units.first().is_some_and(|unit| is_js_trim_unit(*unit)) {
        units = &units[1..];
    }
    while units.last().is_some_and(|unit| is_js_trim_unit(*unit)) {
        units = &units[..units.len() - 1];
    }
    units
}

fn is_js_trim_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

struct HookRegexpContext {
    context: Context,
    _runtime: Runtime,
}

thread_local! {
    static HOOK_REGEXP_CONTEXT: RefCell<Option<HookRegexpContext>> = const { RefCell::new(None) };
}

fn js_regexp_test(
    pattern: &Utf16JsonProjection,
    value: &Utf16JsonProjection,
) -> Result<bool, String> {
    let input = Utf16JsonProjection::array(vec![pattern.clone(), value.clone()])
        .map_err(|error| error.to_string())?
        .to_json_string()
        .map_err(|error| error.to_string())?;
    let program = format!(
        "(()=>{{const [pattern, value] = JSON.parse({}); return new RegExp(pattern).test(value);}})()",
        js_string_literal(&input)
    );
    eval_hook_regexp(&program)
}

pub(crate) fn is_valid_hook_regexp_projection(pattern: &Utf16JsonProjection) -> bool {
    if pattern.string_units("").is_none() {
        return false;
    }
    let input = Utf16JsonProjection::array(vec![pattern.clone()])
        .and_then(|projection| projection.to_json_string())
        .unwrap_or_default();
    let program = format!(
        "(()=>{{const [pattern] = JSON.parse({}); new RegExp(pattern); return true;}})()",
        js_string_literal(&input)
    );
    eval_hook_regexp(&program).unwrap_or(false)
}

fn eval_hook_regexp(program: &str) -> Result<bool, String> {
    HOOK_REGEXP_CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let runtime = Runtime::new().map_err(|error| error.to_string())?;
            let context = Context::full(&runtime).map_err(|error| error.to_string())?;
            *slot = Some(HookRegexpContext {
                context,
                _runtime: runtime,
            });
        }
        let engine = slot
            .as_ref()
            .expect("Hook regex context was just initialized");
        engine
            .context
            .with(|context| context.eval::<bool, _>(program))
            .map_err(|error| error.to_string())
    })
}

fn js_string_literal(value: &str) -> String {
    serde_json::to_string(value)
        .expect("Rust strings are valid JSON strings")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Evaluate a Hook per-entry if condition using its exact source string.
/// Permission-rule parsing keeps the content units intact, then dispatches the
/// same Bash or file/search matcher used by the current Hook input path.
/// Missing/non-string tool fields remain non-matches.
#[must_use]
pub fn matches_if_condition(
    if_condition: &str,
    tool_name: &str,
    tool_input: &Utf16JsonProjection,
) -> bool {
    matches_if_condition_projection(
        &Utf16JsonProjection::from(if_condition),
        tool_name,
        tool_input,
    )
}

/// The rule parser retains arbitrary content units; the tool name remains a
/// typed identifier and must decode as valid Rust text.
pub(crate) fn matches_if_condition_projection(
    if_condition: &Utf16JsonProjection,
    tool_name: &str,
    tool_input: &Utf16JsonProjection,
) -> bool {
    let Some(parsed) = PermissionRuleValue::from_rule_projection(if_condition) else {
        return false;
    };
    let canonical_tool = normalize_legacy_tool_name(tool_name);
    if parsed.tool_name != canonical_tool {
        return false;
    }
    let Some(content_units) = parsed.rule_content_units() else {
        return true;
    };
    matches_content_rule_units(&canonical_tool, &content_units, tool_input)
}

fn matches_content_rule_units(
    tool_name: &str,
    content_units: &[u16],
    input: &Utf16JsonProjection,
) -> bool {
    match tool_name {
        "Bash" => input
            .get_property("command")
            .and_then(|value| value.code_units())
            .is_some_and(|command| {
                rule_matches_any_subcommand_with_candidate_units(content_units, command, &[])
            }),
        "Edit" | "Write" | "Read" => {
            matches_source_wildcard_field(content_units, input, "file_path")
        }
        "Glob" | "Grep" => matches_source_wildcard_field(content_units, input, "pattern"),
        _ => false,
    }
}

fn matches_source_wildcard_field(
    pattern: &[u16],
    input: &Utf16JsonProjection,
    field: &str,
) -> bool {
    input
        .get_property(field)
        .and_then(|value| value.code_units())
        .is_some_and(|units| match_wildcard_pattern_utf16(pattern, units))
}

/// True when `s` is non-empty and every byte is in the "simple pattern" char
/// class — the Rust equivalent of the JS `(n ? /^[a-zA-Z0-9_|, -]+$/ :
/// /^[a-zA-Z0-9_|]+$/).test(s)` test. In comma-mode the class additionally
/// admits comma, space, and (since v2.1.195) the hyphen.
fn is_simple_pattern(s: &str, comma_mode: bool) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || b == b'_'
                || b == b'|'
                || (comma_mode && (b == b',' || b == b' ' || b == b'-'))
        })
}

/// Returns `true` when `matcher` is a simple comma-mode list containing a
/// bare MCP server matcher (`mcp__<server>`) with no tool suffix.
///
/// This supports the existing deprecation warning that suggests
/// `mcp__server__.*` when a matcher names only an MCP server.
#[must_use]
pub fn is_bare_mcp_server_matcher(matcher: &str) -> bool {
    if !is_simple_pattern(matcher, true) {
        return false;
    }
    matcher.split(['|', ',']).map(str::trim).any(|segment| {
        segment
            .strip_prefix("mcp__")
            .is_some_and(|rest| !rest.contains("__"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_empty_match_everything() {
        // `*` and empty short-circuit to true regardless of query.
        assert!(matches_pattern("Write", "*"));
        assert!(matches_pattern("Bash", "*"));
        assert!(matches_pattern("anything", ""));
        assert!(matches_pattern("", ""));
    }

    #[test]
    fn simple_exact_match() {
        assert!(matches_pattern("Write", "Write"));
        assert!(!matches_pattern("Bash", "Write"));
        assert!(!matches_pattern("Writer", "Write"));
        assert!(!matches_pattern("Writ", "Write"));
    }

    #[test]
    fn pipe_separated_exact_match() {
        assert!(matches_pattern("Write", "Write|Edit"));
        assert!(matches_pattern("Edit", "Write|Edit"));
        assert!(!matches_pattern("Bash", "Write|Edit"));
        // Three-way pipe list.
        assert!(matches_pattern("Read", "Write|Edit|Read"));
        // A matcher containing spaces is NOT a simple pattern (`^[A-Za-z0-9_|]+$`
        // rejects the space), so it falls through to the regex branch — same as
        // TS. There it is the alternation `Write ` | ` Edit`, neither of which
        // is a substring of the bare query "Edit", so it does not match.
        assert!(!matches_pattern("Edit", "Write | Edit"));
    }

    #[test]
    fn regex_branch_anchored_and_unanchored() {
        // `^Bash.*` is not a simple pattern (contains `^` and `.` and `*`),
        // so it falls through to the regex branch.
        assert!(matches_pattern("Bash", "^Bash.*"));
        assert!(matches_pattern("BashOutput", "^Bash.*"));
        assert!(!matches_pattern("MultiBash", "^Bash.*"));
        // A pattern with a `.` is NOT simple, so it hits the regex branch and
        // matches unanchored (substring), like JS `RegExp.test`.
        assert!(matches_pattern("MultiBash", "Bash.*"));
        assert!(matches_pattern("BashX", "Bash.*"));
        // But a plain "Bash" is a SIMPLE pattern → exact match, not substring:
        // it must NOT match "MultiBash".
        assert!(!matches_pattern("MultiBash", "Bash"));
    }

    #[test]
    fn source_matcher_preserves_lone_and_paired_surrogate_code_units() {
        let lone = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
        assert!(matches_pattern_projection_with(&lone, "^.$", false, None));
        assert!(matches_pattern_projection_with(
            &lone,
            r"^\uD800$",
            false,
            None
        ));
        assert!(!matches_pattern_projection_with(&lone, "^..$", false, None));

        let paired = Utf16JsonProjection::parse(r#""😀""#).unwrap();
        assert!(matches_pattern_projection_with(
            &paired, "^..$", false, None
        ));
        assert!(!matches_pattern_projection_with(
            &paired, "^.$", false, None
        ));
    }

    #[test]
    fn source_matcher_preserves_cross_ascii_non_ascii_ranges_and_line_terminators() {
        let first_non_ascii = Utf16JsonProjection::parse(r#""\u0080""#).unwrap();
        let next_non_ascii = Utf16JsonProjection::parse(r#""\u0081""#).unwrap();
        let range = "^[A-\u{80}]$";
        assert!(matches_pattern_projection_with(
            &Utf16JsonProjection::from("A"),
            range,
            false,
            None
        ));
        assert!(matches_pattern_projection_with(
            &first_non_ascii,
            range,
            false,
            None
        ));
        assert!(matches_pattern_projection_with(
            &first_non_ascii,
            r"^[A-\x80]$",
            false,
            None
        ));
        assert!(!matches_pattern_projection_with(
            &next_non_ascii,
            range,
            false,
            None
        ));

        let line_separator = Utf16JsonProjection::parse(r#""\u2028""#).unwrap();
        assert!(!matches_pattern_projection_with(
            &line_separator,
            "^.$",
            false,
            None
        ));

        let nbsp = Utf16JsonProjection::parse(r#""\u00a0""#).unwrap();
        let nel = Utf16JsonProjection::parse(r#""\u0085""#).unwrap();
        assert!(matches_pattern_projection_with(&nbsp, r"^\s$", false, None));
        assert!(matches_pattern_projection_with(
            &nbsp, r"^[\s]$", false, None
        ));
        assert!(!matches_pattern_projection_with(&nel, r"^\s$", false, None));

        let feff = Utf16JsonProjection::parse(r#""\ufeff""#).unwrap();
        assert!(matches_pattern_projection_with(&feff, r"^\s$", false, None));

        // Private-use source scalars remain literal values rather than
        // colliding with any temporary marker alphabet.
        let pua = Utf16JsonProjection::parse(r#""\ue000""#).unwrap();
        let pua_neighbor = Utf16JsonProjection::parse(r#""\ue001""#).unwrap();
        assert!(matches_pattern_projection_with(
            &pua,
            "^[\u{e000}-\u{e001}]$",
            false,
            None
        ));
        assert!(!matches_pattern_projection_with(
            &pua_neighbor,
            "^[\u{e000}-\u{e000}]$",
            false,
            None
        ));
    }

    #[test]
    fn regex_branch_uses_javascript_lookaround_and_backreferences() {
        assert!(matches_pattern("BashTool", r"Bash(?=Tool)"));
        assert!(matches_pattern("aa", r"^(.)\1$"));
        assert!(!matches_pattern("ab", r"^(.)\1$"));
        // Rust-only inline flags are invalid in the Native RegExp constructor.
        assert!(!matches_pattern("Bash", r"(?i)Bash"));
    }

    #[test]
    fn regex_branch_matches_source_query_with_exact_surrogate_units() {
        let query = Utf16JsonProjection::parse(r#""\ud800Tool""#).unwrap();
        assert!(matches_pattern_projection_with(
            &query,
            r"\uD800Tool",
            false,
            None
        ));
        assert!(!matches_pattern_projection_with(
            &query,
            r"\uD801Tool",
            false,
            None
        ));
    }

    #[test]
    fn regex_anchored_full_word() {
        assert!(matches_pattern("Task", "^Task$"));
        assert!(!matches_pattern("TaskOutput", "^Task$"));
        assert!(!matches_pattern("MyTask", "^Task$"));
    }

    #[test]
    fn invalid_regex_returns_false_no_panic() {
        // Unbalanced bracket / paren — invalid regex must NOT panic.
        assert!(!matches_pattern("Bash", "^Bash[")); // contains `[` -> regex branch
        assert!(!matches_pattern("Bash", "(unclosed"));
        assert!(!matches_pattern("Bash", "a{2,1}")); // invalid quantifier range
    }

    #[test]
    fn legacy_aliases_resolve_to_canonical() {
        // Forward map (permissionRuleParser.ts LEGACY_TOOL_NAME_ALIASES).
        assert_eq!(normalize_legacy_tool_name("Task"), "Task");
        assert_eq!(normalize_legacy_tool_name("KillShell"), "TaskStop");
        assert_eq!(normalize_legacy_tool_name("AgentOutputTool"), "TaskOutput");
        assert_eq!(normalize_legacy_tool_name("BashOutputTool"), "TaskOutput");
        // Non-legacy names pass through unchanged.
        assert_eq!(normalize_legacy_tool_name("Write"), "Write");
        assert_eq!(normalize_legacy_tool_name("Agent"), "Agent");
        // Reverse map (insertion order matters for the two-alias TaskOutput).
        assert!(get_legacy_tool_names("Agent").is_empty());
        assert_eq!(
            get_legacy_tool_names("TaskStop"),
            vec!["KillShell".to_string()]
        );
        assert_eq!(
            get_legacy_tool_names("TaskOutput"),
            vec!["AgentOutputTool".to_string(), "BashOutputTool".to_string()]
        );
        assert!(get_legacy_tool_names("Write").is_empty());
    }

    #[test]
    fn removed_agent_alias_does_not_match_canonical_tool() {
        assert!(!matches_pattern("Agent", "Task"));
        assert!(!matches_pattern("Agent", "^Task$"));
        assert!(!matches_pattern("Agent", "Read|Task"));
        assert!(matches_pattern("Task", "Task"));
        // Unrelated tool aliases retain their existing matching behavior.
        assert!(matches_pattern("TaskStop", "KillShell|Write"));
    }

    // ── `if`-condition matcher (prepareIfConditionMatcher + per-tool) ──────────

    macro_rules! json {
        ($($tokens:tt)+) => {
            Utf16JsonProjection::from_serde_value(&serde_json::json!($($tokens)+)).unwrap()
        };
    }

    #[test]
    fn if_bash_prefix_rule_fires_on_matching_command() {
        let input = json!({ "command": "git push origin main" });
        assert!(matches_if_condition("Bash(git push:*)", "Bash", &input));
    }

    #[test]
    fn if_bash_prefix_rule_does_not_fire_on_non_matching_command() {
        let input = json!({ "command": "git status" });
        assert!(!matches_if_condition("Bash(git push:*)", "Bash", &input));
        // A different command entirely.
        let ls = json!({ "command": "ls -la" });
        assert!(!matches_if_condition("Bash(git push:*)", "Bash", &ls));
    }

    #[test]
    fn if_bash_fires_when_any_subcommand_matches() {
        // Deny-like aggregation: a compound command fires the hook if ANY
        // subcommand matches (BashTool.tsx comment).
        let input = json!({ "command": "echo ok && git push" });
        assert!(matches_if_condition("Bash(git push:*)", "Bash", &input));
        // None of the subcommands match → no fire.
        let benign = json!({ "command": "echo ok && ls" });
        assert!(!matches_if_condition("Bash(git push:*)", "Bash", &benign));
    }

    #[test]
    fn if_bash_wildcard_rule() {
        let input = json!({ "command": "git commit -m x" });
        assert!(matches_if_condition("Bash(git *)", "Bash", &input));
        // word boundary: `git *` must not match `gitk`.
        let gitk = json!({ "command": "gitk" });
        assert!(!matches_if_condition("Bash(git *)", "Bash", &gitk));
    }

    #[test]
    fn if_bare_tool_name_matches_on_tool_agreement_alone() {
        // No rule content → matches purely on the tool name (TS `!parsed.ruleContent`).
        let input = json!({ "command": "rm -rf /" });
        assert!(matches_if_condition("Bash", "Bash", &input));
        // `Bash()` and `Bash(*)` collapse to tool-wide too.
        assert!(matches_if_condition("Bash()", "Bash", &input));
        assert!(matches_if_condition("Bash(*)", "Bash", &input));
    }

    #[test]
    fn if_tool_name_mismatch_does_not_fire() {
        // The rule's tool name must equal the event's tool name.
        let input = json!({ "command": "git push" });
        assert!(!matches_if_condition("Bash(git push:*)", "Write", &input));
        // Bare-tool rule for a different tool also fails.
        assert!(!matches_if_condition("Read", "Bash", &input));
    }

    #[test]
    fn if_removed_agent_alias_does_not_match() {
        let input = json!({ "description": "x" });
        assert!(!matches_if_condition("Task", "Agent", &input));
        assert!(!matches_if_condition("Task(*)", "Agent", &input));
        assert!(!matches_if_condition("Agent", "Task", &input));
        assert!(matches_if_condition("Agent(*)", "Agent", &input));
    }

    #[test]
    fn if_malformed_rule_does_not_fire() {
        // Unbalanced paren: `from_rule_string` treats the WHOLE string as a bare
        // tool name (`"Bash(git push:*"`), which != "Bash" → no fire. Mirrors TS
        // `permissionRuleValueFromString` producing toolName === the whole string.
        let input = json!({ "command": "git push" });
        assert!(!matches_if_condition("Bash(git push:*", "Bash", &input));
        // Content after the close paren → bare tool name `"Bash(x)y"` != "Bash".
        assert!(!matches_if_condition("Bash(x)y", "Bash", &input));
    }

    #[test]
    fn if_file_tool_wildcards_match_raw_path() {
        // matchWildcardPattern against the raw file_path (anchored wildcard).
        let input = json!({ "file_path": "/proj/src/main.rs" });
        assert!(matches_if_condition("Edit(*main.rs)", "Edit", &input));
        assert!(matches_if_condition("Edit(/proj/src/*)", "Edit", &input));
        assert!(matches_if_condition("Write(/proj/*)", "Write", &input));
        assert!(matches_if_condition(
            "Read(/proj/src/main.rs)",
            "Read",
            &input
        ));
        // Non-matching path.
        assert!(!matches_if_condition("Edit(*.py)", "Edit", &input));
        // A bare unanchored relative pattern does NOT match an absolute path
        // (faithful to anchored `^src/.*$`).
        assert!(!matches_if_condition("Edit(src/*)", "Edit", &input));
    }

    #[test]
    fn if_glob_grep_match_search_pattern_not_path() {
        // Glob/Grep match the rule against the SEARCH `pattern` field.
        let glob = json!({ "pattern": "**/*.rs", "path": "/proj" });
        assert!(matches_if_condition("Glob(**/*.rs)", "Glob", &glob));
        let grep = json!({ "pattern": "TODO", "path": "/proj" });
        assert!(matches_if_condition("Grep(TODO)", "Grep", &grep));
        assert!(!matches_if_condition("Grep(FIXME)", "Grep", &grep));
    }

    #[test]
    fn if_content_rule_on_unmatchable_tool_does_not_fire() {
        // PowerShell / NotebookEdit have no preparePermissionMatcher in TS, so a
        // CONTENT rule never matches (patternMatcher undefined → false). The bare
        // tool-name form still matches on tool agreement.
        let ps = json!({ "command": "Get-ChildItem" });
        assert!(!matches_if_condition(
            "PowerShell(Get-ChildItem:*)",
            "PowerShell",
            &ps
        ));
        assert!(matches_if_condition("PowerShell", "PowerShell", &ps));
        let nb = json!({ "notebook_path": "/a.ipynb" });
        assert!(!matches_if_condition(
            "NotebookEdit(/a.ipynb)",
            "NotebookEdit",
            &nb
        ));
        assert!(matches_if_condition("NotebookEdit", "NotebookEdit", &nb));
    }

    #[test]
    fn if_file_and_search_wildcards_match_unpaired_source_units() {
        let file = Utf16JsonProjection::parse(r#"{"file_path":"/proj/\ud800.rs"}"#).unwrap();
        assert!(file.string_units("/file_path").unwrap().contains(&0xd800));
        assert!(matches_if_condition("Edit(*.rs)", "Edit", &file));
        assert!(!matches_if_condition("Edit(*.txt)", "Edit", &file));
        assert!(matches_if_condition("Edit(/proj/**/*.rs)", "Edit", &file));

        let glob = Utf16JsonProjection::parse(r#"{"pattern":"src/\ud800/*.rs"}"#).unwrap();
        assert!(matches_if_condition("Glob(src/*/*.rs)", "Glob", &glob));
        assert!(!matches_if_condition("Glob(src/*/*.txt)", "Glob", &glob));

        let paired = Utf16JsonProjection::parse(r#"{"file_path":"/proj/😀.rs"}"#).unwrap();
        assert!(matches_if_condition("Edit(*😀.rs)", "Edit", &paired));
        let pua = Utf16JsonProjection::parse(r#"{"file_path":"/proj/\ue000.rs"}"#).unwrap();
        let pua_rule = Utf16JsonProjection::parse(r#""Edit(*\ue000.rs)""#).unwrap();
        let neighbor_rule = Utf16JsonProjection::parse(r#""Edit(*\ue001.rs)""#).unwrap();
        assert!(matches_if_condition_projection(&pua_rule, "Edit", &pua));
        assert!(!matches_if_condition_projection(
            &neighbor_rule,
            "Edit",
            &pua
        ));

        // Pattern trimming follows JavaScript String.trim: FEFF is removed,
        // while NEL (U+0085) remains a literal and therefore does not match.
        let plain_path = Utf16JsonProjection::parse(r#"{"file_path":"/proj/file"}"#).unwrap();
        let feff_rule = Utf16JsonProjection::parse(r#""Edit(\ufeff/proj/file)""#).unwrap();
        let nel_rule = Utf16JsonProjection::parse(r#""Edit(\u0085/proj/file)""#).unwrap();
        assert!(matches_if_condition_projection(
            &feff_rule,
            "Edit",
            &plain_path
        ));
        assert!(!matches_if_condition_projection(
            &nel_rule,
            "Edit",
            &plain_path
        ));

        let command = Utf16JsonProjection::parse(r#"{"command":"git push \ud800"}"#).unwrap();
        assert!(matches_if_condition("Bash(git push:*)", "Bash", &command));
        // Bare tool-name rules do not inspect a string field and still match.
        assert!(matches_if_condition("Bash", "Bash", &command));
    }

    #[test]
    fn source_if_rule_and_shell_command_keep_unpaired_units() {
        let if_rule = Utf16JsonProjection::parse(r#""Bash(git \ud800:*)""#).unwrap();
        let command = Utf16JsonProjection::parse(r#"{"command":"git \ud800 push"}"#).unwrap();
        assert!(matches_if_condition_projection(&if_rule, "Bash", &command));

        let other_command = Utf16JsonProjection::parse(r#"{"command":"git \ud801 push"}"#).unwrap();
        assert!(!matches_if_condition_projection(
            &if_rule,
            "Bash",
            &other_command
        ));

        let file_rule = Utf16JsonProjection::parse(r#""Edit(*\ud800.rs)""#).unwrap();
        let file = Utf16JsonProjection::parse(r#"{"file_path":"/proj/\ud800.rs"}"#).unwrap();
        assert!(matches_if_condition_projection(&file_rule, "Edit", &file));

        let wrong_rule = Utf16JsonProjection::parse(r#""Edit(*\ud801.rs)""#).unwrap();
        assert!(!matches_if_condition_projection(&wrong_rule, "Edit", &file));
    }

    #[test]
    fn if_content_rule_with_missing_input_field_does_not_fire() {
        // Missing the matched field ⇒ TS input-schema parse fails ⇒ undefined
        // matcher ⇒ false. (A bare tool-name rule still matches.)
        let empty = json!({});
        assert!(!matches_if_condition("Bash(git push:*)", "Bash", &empty));
        assert!(!matches_if_condition("Edit(*.rs)", "Edit", &empty));
        assert!(matches_if_condition("Bash", "Bash", &empty));
    }

    // ── comma-mode matcher (`matchesPattern` `n`/`r`, claude 2.1.195) ──────────

    #[test]
    fn comma_mode_splits_on_comma_and_pipe() {
        // In comma-mode a list may use commas, pipes, or both, with spaces.
        assert!(matches_pattern_with("Edit", "Write, Edit", true, None));
        assert!(matches_pattern_with(
            "Read",
            "Write | Edit | Read",
            true,
            None
        ));
        assert!(matches_pattern_with(
            "Read",
            "Write, Edit | Read",
            true,
            None
        ));
        assert!(!matches_pattern_with("Bash", "Write, Edit", true, None));
        // Empty segments (trailing/double separators) are dropped, not matched.
        assert!(matches_pattern_with("Edit", "Write,,Edit,", true, None));
        assert!(!matches_pattern_with("", "Write,,Edit,", true, None));
    }

    #[test]
    fn comma_mode_off_keeps_comma_as_regex() {
        // Without comma-mode a comma makes the pattern NON-simple, so it falls
        // through to the regex branch — exactly the classic behavior. `Write,
        // Edit` as a regex does not match the bare query "Edit".
        assert!(!matches_pattern_with("Edit", "Write, Edit", false, None));
        // The 2-arg wrapper is comma-mode OFF.
        assert!(!matches_pattern("Edit", "Write, Edit"));
    }

    #[test]
    fn comma_mode_hyphen_is_simple_match() {
        // The v2.1.195 hyphen: a hyphenated name (e.g. an MCP server) is a
        // SIMPLE list entry in comma-mode (exact match), not a regex.
        assert!(matches_pattern_with(
            "mcp__my-server",
            "mcp__my-server",
            true,
            None
        ));
        assert!(matches_pattern_with(
            "mcp__my-server",
            "Bash, mcp__my-server",
            true,
            None
        ));
        // Exact, not substring (simple pattern ⇒ exact).
        assert!(!matches_pattern_with(
            "mcp__my-server__tool",
            "mcp__my-server",
            true,
            None
        ));
        // With comma-mode OFF the hyphen pattern is NOT simple → regex branch,
        // where `mcp__my-server` matches as an unanchored substring.
        assert!(matches_pattern_with(
            "x_mcp__my-server_y",
            "mcp__my-server",
            false,
            None
        ));
    }

    #[test]
    fn comma_mode_tool_aliases_expand_segments() {
        // `r` extra map: a matcher segment that the alias map remaps also
        // matches the mapped name (`ofn(xP(i), r)` → [name, mapped]).
        let mut aliases = HashMap::new();
        aliases.insert("Deploy".to_string(), "Bash".to_string());
        // Matcher "Deploy" → normalized "Deploy" → alias map → "Bash": matches a
        // "Bash" query.
        assert!(matches_pattern_with("Bash", "Deploy", true, Some(&aliases)));
        // Still matches the literal name too.
        assert!(matches_pattern_with(
            "Deploy",
            "Deploy",
            true,
            Some(&aliases)
        ));
        // Unrelated query does not match.
        assert!(!matches_pattern_with(
            "Read",
            "Deploy",
            true,
            Some(&aliases)
        ));
        // Regex branch reverse lookup (`sfn`): a regex over the query "Bash"
        // also matches the alias key "Deploy" that maps to it.
        assert!(matches_pattern_with(
            "Bash",
            "^Deploy$",
            true,
            Some(&aliases)
        ));
    }

    #[test]
    fn is_bare_mcp_server_matcher_detects_suffixless_mcp() {
        // `mcp__server` with no `__tool` suffix is "bare" (deprecation target).
        assert!(is_bare_mcp_server_matcher("mcp__github"));
        assert!(is_bare_mcp_server_matcher("Bash, mcp__github"));
        assert!(is_bare_mcp_server_matcher("mcp__my-server")); // hyphen ok
                                                               // A fully-qualified MCP tool matcher is NOT bare.
        assert!(!is_bare_mcp_server_matcher("mcp__github__create_issue"));
        // Non-MCP matchers are never bare.
        assert!(!is_bare_mcp_server_matcher("Write|Edit"));
        // A regex (non-simple) matcher is never classified as bare.
        assert!(!is_bare_mcp_server_matcher("^mcp__github$"));
    }
}
