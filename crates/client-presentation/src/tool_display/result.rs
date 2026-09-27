//! The `⎿` result line and the expandable result body.
//!
//! Every field path below was read off the tool's own construction site (the
//! `ToolCallResult.data` literal), not inferred from a name. The value these
//! functions receive IS `ToolCallResult.data` — `turn_loop.rs` passes
//! `result.data` straight through to `emit_tool_result`, so there is no `data`
//! wrapper to unwrap here.

use serde_json::Value;

use crate::render::diff;

/// Body line count past which a result should start collapsed.
pub const INLINE_BODY_BUDGET: usize = 10;

/// Structured-diff row count past which a result should start collapsed.
///
/// The companion to [`INLINE_BODY_BUDGET`] for the OTHER thing a result row
/// mounts. A `MAX_WIRE_DIFF_ROWS`-sized diff beside a one-line body is still
/// hundreds of rows on screen, so the wire verdict has to weigh both — the
/// Electron desktop had already had to invent this budget locally because the
/// engine's `collapsed` only ever looked at the body.
pub const INLINE_DIFF_ROW_BUDGET: usize = 60;

/// Cap on the body text shipped to clients, in bytes.
///
/// Clients retain dozens of tool rows; an uncapped `cargo build` log across
/// all of them is a multi-megabyte resident heap on a phone. The untruncated
/// text always remains in the result payload itself.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Cap on the body text shipped to clients, in lines.
pub const MAX_BODY_LINES: usize = 2_000;

/// claude-code's `Added N line(s)[, removed M line(s)]` edit summary.
///
/// "Removed" is capitalized only when it is the sole clause. Moved verbatim
/// from `tui/src/history_cell/tool.rs`.
#[must_use]
pub fn added_removed_header(additions: usize, removals: usize) -> Option<String> {
    let added = (additions > 0).then(|| {
        format!(
            "Added {additions} {}",
            if additions > 1 { "lines" } else { "line" }
        )
    });
    let removed = (removals > 0).then(|| {
        let cap = if additions == 0 { "R" } else { "r" };
        format!(
            "{cap}emoved {removals} {}",
            if removals > 1 { "lines" } else { "line" }
        )
    });
    match (added, removed) {
        (Some(a), Some(r)) => Some(format!("{a}, {r}")),
        (Some(a), None) => Some(a),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    }
}

/// Whether a tool-result payload represents an error.
///
/// The engine shapes every failure as `{ "error": … }` (`turn_loop.rs`), so
/// the check is structural. Moved out of `client-adapter`'s private
/// `AdapterOutputStream::result_is_error` so the terminal and the wire agree.
#[must_use]
pub fn result_is_error(result: &Value) -> bool {
    result.get("error").is_some()
}

/// The first non-empty line of `text`, trimmed.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim_end)
        .find(|line| !line.trim().is_empty())
        .map(str::to_string)
}

/// A `usize` field, if present and non-negative.
fn count_field(value: &Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// Pluralize `noun` for `n` by appending a bare "s".
fn plural(n: usize, noun: &str) -> String {
    plural_with(n, noun, &format!("{noun}s"))
}

/// Pluralize with an EXPLICIT plural form, for the nouns a bare "s" gets wrong
/// ("Found 2 matchs").
fn plural_with(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// What kind of thing a result headline says.
///
/// Localizing clients key their copy off this and substitute [`counts`]; the
/// terminal and the Electron desktop, neither of which localizes, render
/// [`english`] directly. Shipping only the English string would put "Added 18
/// lines" into a zh-Hans build — a real regression against the localized
/// `chat_tool_*` copy those clients already have.
///
/// [`counts`]: ResultHeadline::counts
/// [`english`]: ResultHeadline::english
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlineKind {
    /// `counts = [additions]`.
    Added,
    /// `counts = [removals]`.
    Removed,
    /// `counts = [additions, removals]`.
    AddedRemoved,
    /// `counts = [read]`.
    LinesRead,
    /// `counts = [read, total]` — a partial read.
    LinesReadPartial,
    /// `counts = [n]`.
    FilesFound,
    /// `counts = [n]` — files found, but the search hit its cap.
    FilesFoundTruncated,
    /// `counts = [n]`.
    LinesFound,
    /// `counts = [n]`.
    MatchesFound,
    /// A command was interrupted.
    Interrupted,
    /// A command produced no output at all.
    NoContent,
    /// A failure; the message is in `text`.
    Failed,
    /// Free text with no numbers — the message is in `text`.
    Plain,
}

impl HeadlineKind {
    /// A stable, non-localized lookup key.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::AddedRemoved => "added_removed",
            Self::LinesRead => "lines_read",
            Self::LinesReadPartial => "lines_read_partial",
            Self::FilesFound => "files_found",
            Self::FilesFoundTruncated => "files_found_truncated",
            Self::LinesFound => "lines_found",
            Self::MatchesFound => "matches_found",
            Self::Interrupted => "interrupted",
            Self::NoContent => "no_content",
            Self::Failed => "failed",
            Self::Plain => "plain",
        }
    }
}

/// A result headline in both localizable and rendered form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultHeadline {
    /// What the headline says.
    pub kind: HeadlineKind,
    /// Numeric slots, in the order [`HeadlineKind`] documents.
    pub counts: Vec<u32>,
    /// Free-text slot ([`HeadlineKind::Plain`] / [`HeadlineKind::Failed`]).
    pub text: Option<String>,
    /// The rendered English form.
    pub english: String,
}

/// The `⎿` headline for one completed tool call, rendered to English.
///
/// A thin wrapper over [`result_headline_parts`] for the terminal.
#[must_use]
pub fn result_headline(
    tool: &str,
    input: Option<&Value>,
    result: &Value,
    is_error: bool,
) -> Option<String> {
    result_headline_parts(tool, input, result, is_error).map(|headline| headline.english)
}

/// The `⎿` headline for one completed tool call.
///
/// `input` is the originating call input when the surface retained it; the
/// edit tools derive their line counts from it so the headline and the diff
/// rendered beneath it always describe the same comparison. When it is
/// absent the result payload's own `originalFile`/`newString` pair is used.
///
/// Returns `None` when there is nothing worth saying (TodoWrite, whose
/// checklist renders instead).
#[must_use]
pub fn result_headline_parts(
    tool: &str,
    input: Option<&Value>,
    result: &Value,
    is_error: bool,
) -> Option<ResultHeadline> {
    let plain = |kind: HeadlineKind, text: String| ResultHeadline {
        kind,
        counts: Vec::new(),
        english: text.clone(),
        text: Some(text),
    };
    let counted = |kind: HeadlineKind, counts: Vec<u32>, english: String| ResultHeadline {
        kind,
        counts,
        text: None,
        english,
    };

    if is_error {
        let message = result
            .get("error")
            .and_then(Value::as_str)
            .and_then(first_line)
            .unwrap_or_else(|| "Failed".to_string());
        return Some(plain(HeadlineKind::Failed, message));
    }
    // RESUME: a persisted transcript carries a tool result's MODEL-FACING
    // STRING (`ToolCallResult.model_content`, or `turn_loop`'s
    // `tool_result_to_model_text` when a tool has none) — never the structured
    // `ToolCallResult.data` object the LIVE path passes. Every arm below that
    // indexes `result` as an object therefore saw nothing on a resumed
    // transcript: Bash headlined "(No content)" and Read lost its line count
    // entirely. Recover what the string still carries.
    //
    // The edit family and the `_` catch-all are deliberately absent here: the
    // former reads the retained CALL INPUT via `edit_sides` (resume does pair
    // the call), and the latter already handles a bare string through
    // `result_body`/`generic_body`.
    if let Some(text) = result.as_str() {
        match tool {
            "Read" => {
                // `read.rs` puts the cat -n render on `model_content`, one
                // source line per output line.
                let read = text.lines().count();
                return Some(counted(
                    HeadlineKind::LinesRead,
                    vec![as_u32(read)],
                    format!("Read {}", plural(read, "line")),
                ));
            }
            "Bash" | "Shell" | "PowerShell" | "Grep" | "Glob" => {
                return Some(match first_line(text) {
                    Some(line) => plain(HeadlineKind::Plain, line),
                    None => counted(
                        HeadlineKind::NoContent,
                        Vec::new(),
                        "(No content)".to_string(),
                    ),
                });
            }
            _ => {}
        }
    }
    match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let (old, new) = edit_sides(tool, input, result)?;
            let (additions, removals) = diff::diff_stats(&old, &new);
            let english = added_removed_header(additions, removals)?;
            let (kind, counts) = match (additions, removals) {
                (0, 0) => return None,
                (_, 0) => (HeadlineKind::Added, vec![as_u32(additions)]),
                (0, _) => (HeadlineKind::Removed, vec![as_u32(removals)]),
                _ => (
                    HeadlineKind::AddedRemoved,
                    vec![as_u32(additions), as_u32(removals)],
                ),
            };
            Some(counted(kind, counts, english))
        }
        "Read" => {
            let file = result.get("file")?;
            let read = count_field(file, "numLines")?;
            let total = count_field(file, "totalLines");
            let mut english = format!("Read {}", plural(read, "line"));
            let partial = total.is_some_and(|total| total > read);
            if partial {
                english.push_str(&format!(" (of {})", total.unwrap_or(read)));
                return Some(counted(
                    HeadlineKind::LinesReadPartial,
                    vec![as_u32(read), as_u32(total.unwrap_or(read))],
                    english,
                ));
            }
            Some(counted(
                HeadlineKind::LinesRead,
                vec![as_u32(read)],
                english,
            ))
        }
        "Grep" => {
            let mode = result.get("mode").and_then(Value::as_str).unwrap_or("");
            match mode {
                "content" => count_field(result, "numLines").map(|n| {
                    counted(
                        HeadlineKind::LinesFound,
                        vec![as_u32(n)],
                        format!("Found {}", plural(n, "line")),
                    )
                }),
                "count" => count_field(result, "numMatches")
                    .or_else(|| count_field(result, "numFiles"))
                    .map(|n| {
                        counted(
                            HeadlineKind::MatchesFound,
                            vec![as_u32(n)],
                            format!("Found {}", plural_with(n, "match", "matches")),
                        )
                    }),
                _ => count_field(result, "numFiles").map(|n| {
                    counted(
                        HeadlineKind::FilesFound,
                        vec![as_u32(n)],
                        format!("Found {}", plural(n, "file")),
                    )
                }),
            }
        }
        "Glob" => {
            let found = count_field(result, "numFiles")?;
            let truncated = result.get("truncated").and_then(Value::as_bool) == Some(true);
            let mut english = format!("Found {}", plural(found, "file"));
            if truncated {
                english.push_str(" (truncated)");
            }
            Some(counted(
                if truncated {
                    HeadlineKind::FilesFoundTruncated
                } else {
                    HeadlineKind::FilesFound
                },
                vec![as_u32(found)],
                english,
            ))
        }
        "Bash" | "Shell" | "PowerShell" => {
            if result.get("interrupted").and_then(Value::as_bool) == Some(true) {
                return Some(counted(
                    HeadlineKind::Interrupted,
                    Vec::new(),
                    "Interrupted".to_string(),
                ));
            }
            let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
            match first_line(stdout).or_else(|| first_line(stderr)) {
                Some(line) => Some(plain(HeadlineKind::Plain, line)),
                None => Some(counted(
                    HeadlineKind::NoContent,
                    Vec::new(),
                    "(No content)".to_string(),
                )),
            }
        }
        // The checklist itself is the presentation.
        "TodoWrite" => None,
        // No headline rule: the body flattened onto one line. Newlines become
        // single spaces rather than the body being cut at its first line —
        // that keeps a short multi-line result fully readable in the one-line
        // slot, and matches how the terminal has always flattened it.
        _ => result_body(tool, result)
            .map(|body| plain(HeadlineKind::Plain, body.replace('\n', " "))),
    }
}

/// Widen a count for the wire, saturating rather than panicking.
fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The `(old, new)` pair an edit tool's diff compares.
///
/// Prefers the call input so the headline matches the diff the surface draws
/// beside it; falls back to the result payload, which carries the truth even
/// when the input was not retained.
fn edit_sides(tool: &str, input: Option<&Value>, result: &Value) -> Option<(String, String)> {
    if let Some(input) = input {
        let (old, new, _path) = crate::tool_display::diff_inputs_for(tool, input);
        if old.is_some() || new.is_some() {
            return Some((old.unwrap_or_default(), new.unwrap_or_default()));
        }
    }
    let original = result
        .get("originalFile")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Edit reports `newString`; Write reports `content`.
    let updated = result
        .get("newString")
        .or_else(|| result.get("content"))
        .and_then(Value::as_str)?;
    Some((original.to_string(), updated.to_string()))
}

/// The plain-text body a client shows when a result is expanded.
///
/// Not truncated here — [`clamp_body`] applies the wire caps, so a terminal
/// that wants the whole thing can still have it.
#[must_use]
pub fn result_body(tool: &str, result: &Value) -> Option<String> {
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return Some(error.to_string());
    }
    match tool {
        // The structured DIFF is the body for the edit family. Their
        // `ToolCallResult.data` — `{filePath, oldString, newString,
        // originalFile, structuredPatch, userModified, replaceAll}`
        // (`tools/file/src/edit.rs`) — shares NO key with `generic_body`'s
        // probes, so falling through used to dump the whole compact JSON,
        // pre-edit file included, into the user-visible body (measured 32 KB
        // for a 3000-line file). Compact JSON is also ONE line, so
        // `clamp_body` reported `collapsed: false` and every GUI client
        // mounted the blob inline.
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => None,
        "Bash" | "Shell" | "PowerShell" => {
            // RESUME: the transcript persists the model-facing string, not the
            // `{stdout, stderr}` object (see `result_headline_parts`).
            if let Some(text) = result.as_str() {
                return (!text.is_empty()).then(|| text.to_string());
            }
            let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
            let joined = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
                (true, true) => String::new(),
                (false, true) => stdout.to_string(),
                (true, false) => stderr.to_string(),
                (false, false) => format!("{stdout}\n{stderr}"),
            };
            (!joined.is_empty()).then_some(joined)
        }
        "Read" => {
            // RESUME: same — the cat -n render arrives as a bare string.
            if let Some(text) = result.as_str() {
                return (!text.is_empty()).then(|| text.to_string());
            }
            result
                .get("file")
                .and_then(|file| file.get("content"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        _ => generic_body(result),
    }
}

/// Body extraction for a tool with no special case: a bare string, then the
/// common text-bearing keys, then a SCALAR as a last resort.
///
/// The last resort is deliberately scalar-only. Serializing an unrecognized
/// object or array here means shipping a tool's whole internal payload as
/// user-visible body text — the shape that made every `Edit` carry its
/// `originalFile` to all four clients. A structured payload that matched none
/// of the probes has no human-readable body; say so with `None` rather than
/// inventing one out of JSON punctuation.
fn generic_body(result: &Value) -> Option<String> {
    if let Some(text) = result.as_str() {
        return Some(text.to_string());
    }
    for key in [
        "content", "output", "result", "message", "summary", "stdout",
    ] {
        if let Some(text) = result.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    match result {
        // `Value::String` is already handled above.
        Value::Number(_) | Value::Bool(_) => Some(result.to_string()),
        // An MCP tool's `data` IS its content-block array — `tools/mcp/src/
        // mcp_tool.rs` sets `let data = content;` with no wrapper. Join the
        // text blocks rather than dumping the array's JSON at the user.
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        Value::Null | Value::String(_) | Value::Object(_) => None,
    }
}

/// A body clamped to the wire caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClampedBody {
    /// The (possibly clamped) text.
    pub text: String,
    /// Line count BEFORE clamping — drives a client's "show N more lines".
    pub total_lines: usize,
    /// The text was clamped.
    pub truncated: bool,
    /// The untruncated body exceeded [`INLINE_BODY_BUDGET`], so a client
    /// should render it collapsed.
    pub collapsed: bool,
}

/// Clamp a result body to [`MAX_BODY_LINES`] / [`MAX_BODY_BYTES`].
#[must_use]
pub fn clamp_body(body: &str) -> ClampedBody {
    let total_lines = body.lines().count();
    let mut text = body.to_string();
    let mut truncated = false;
    if total_lines > MAX_BODY_LINES {
        text = body
            .lines()
            .take(MAX_BODY_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        truncated = true;
    }
    if text.len() > MAX_BODY_BYTES {
        // Cut on a char boundary so the result stays valid UTF-8.
        let mut end = MAX_BODY_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    ClampedBody {
        text,
        total_lines,
        truncated,
        collapsed: total_lines > INLINE_BODY_BUDGET,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn added_removed_header_variants() {
        assert_eq!(added_removed_header(0, 0), None);
        assert_eq!(added_removed_header(1, 0).as_deref(), Some("Added 1 line"));
        assert_eq!(added_removed_header(2, 0).as_deref(), Some("Added 2 lines"));
        // Sole clause capitalizes; paired clause does not.
        assert_eq!(
            added_removed_header(0, 1).as_deref(),
            Some("Removed 1 line")
        );
        assert_eq!(
            added_removed_header(3, 2).as_deref(),
            Some("Added 3 lines, removed 2 lines")
        );
    }

    #[test]
    fn error_results_are_detected_structurally_and_headline_the_first_line() {
        let result = json!({"error": "boom: it broke\nsecond line"});
        assert!(result_is_error(&result));
        assert_eq!(
            result_headline("Bash", None, &result, true).as_deref(),
            Some("boom: it broke")
        );
        assert!(!result_is_error(&json!({"stdout": "fine"})));
    }

    #[test]
    fn edit_headline_uses_the_call_input_when_available() {
        let input = json!({
            "file_path": "/tmp/x.rs",
            "old_string": "a\nb\n",
            "new_string": "a\nB\nc\n",
        });
        assert_eq!(
            result_headline("Edit", Some(&input), &json!({}), false).as_deref(),
            Some("Added 2 lines, removed 1 line")
        );
    }

    #[test]
    fn edit_headline_falls_back_to_the_result_payload() {
        // Shape read off `tools/file/src/edit.rs`'s data literal.
        let result = json!({
            "originalFile": "a\nb\n",
            "newString": "a\nB\nc\n",
            "structuredPatch": [],
        });
        assert_eq!(
            result_headline("Edit", None, &result, false).as_deref(),
            Some("Added 2 lines, removed 1 line")
        );
        // Write reports the new text under `content` instead.
        let result = json!({"originalFile": "", "content": "one\ntwo\n"});
        assert_eq!(
            result_headline("Write", None, &result, false).as_deref(),
            Some("Added 2 lines")
        );
    }

    #[test]
    fn read_headline_reports_lines_and_notes_a_partial_read() {
        // `tools/file/src/read.rs` nests these under `file`.
        let full = json!({"type": "text", "file": {"numLines": 12, "totalLines": 12}});
        assert_eq!(
            result_headline("Read", None, &full, false).as_deref(),
            Some("Read 12 lines")
        );
        let partial = json!({"type": "text", "file": {"numLines": 12, "totalLines": 400}});
        assert_eq!(
            result_headline("Read", None, &partial, false).as_deref(),
            Some("Read 12 lines (of 400)")
        );
        let one = json!({"type": "text", "file": {"numLines": 1, "totalLines": 1}});
        assert_eq!(
            result_headline("Read", None, &one, false).as_deref(),
            Some("Read 1 line")
        );
    }

    #[test]
    fn grep_headline_follows_the_mode() {
        let content = json!({"mode": "content", "numFiles": 2, "numLines": 7});
        assert_eq!(
            result_headline("Grep", None, &content, false).as_deref(),
            Some("Found 7 lines")
        );
        let files = json!({"mode": "files_with_matches", "numFiles": 3});
        assert_eq!(
            result_headline("Grep", None, &files, false).as_deref(),
            Some("Found 3 files")
        );
        let count = json!({"mode": "count", "numMatches": 1});
        assert_eq!(
            result_headline("Grep", None, &count, false).as_deref(),
            Some("Found 1 match")
        );
        // n == 1 is the ONE count that cannot expose a bad plural: the bare-"s"
        // pluralizer used to render this "Found 2 matchs".
        let counts = json!({"mode": "count", "numMatches": 2});
        assert_eq!(
            result_headline("Grep", None, &counts, false).as_deref(),
            Some("Found 2 matches")
        );
        let content_two = json!({"mode": "content", "numLines": 2});
        assert_eq!(
            result_headline("Grep", None, &content_two, false).as_deref(),
            Some("Found 2 lines")
        );
    }

    #[test]
    fn glob_headline_flags_truncation() {
        assert_eq!(
            result_headline(
                "Glob",
                None,
                &json!({"numFiles": 4, "truncated": false}),
                false
            )
            .as_deref(),
            Some("Found 4 files")
        );
        assert_eq!(
            result_headline(
                "Glob",
                None,
                &json!({"numFiles": 100, "truncated": true}),
                false
            )
            .as_deref(),
            Some("Found 100 files (truncated)")
        );
    }

    #[test]
    fn bash_headline_prefers_stdout_then_stderr_then_a_placeholder() {
        let out = json!({"stdout": "\n\ncompiling…\nmore", "stderr": "", "interrupted": false});
        assert_eq!(
            result_headline("Bash", None, &out, false).as_deref(),
            Some("compiling…")
        );
        let err = json!({"stdout": "", "stderr": "warning: unused", "interrupted": false});
        assert_eq!(
            result_headline("Bash", None, &err, false).as_deref(),
            Some("warning: unused")
        );
        let empty = json!({"stdout": "", "stderr": "", "interrupted": false});
        assert_eq!(
            result_headline("Bash", None, &empty, false).as_deref(),
            Some("(No content)")
        );
        let stopped = json!({"stdout": "partial", "stderr": "", "interrupted": true});
        assert_eq!(
            result_headline("Bash", None, &stopped, false).as_deref(),
            Some("Interrupted")
        );
    }

    #[test]
    fn todo_write_has_no_headline() {
        assert!(result_headline("TodoWrite", None, &json!({}), false).is_none());
    }

    #[test]
    fn bash_body_joins_stdout_and_stderr() {
        let both = json!({"stdout": "out", "stderr": "err"});
        assert_eq!(result_body("Bash", &both).as_deref(), Some("out\nerr"));
        let only_out = json!({"stdout": "out", "stderr": ""});
        assert_eq!(result_body("Bash", &only_out).as_deref(), Some("out"));
        let neither = json!({"stdout": "", "stderr": ""});
        assert!(result_body("Bash", &neither).is_none());
    }

    #[test]
    fn generic_headline_flattens_newlines_rather_than_cutting_at_the_first_line() {
        // The terminal has always shown the whole (flattened) summary in the
        // `⎿` slot; cutting at the first line would silently drop content.
        let result = json!("line one\nline two");
        assert_eq!(
            result_headline("Whatever", None, &result, false).as_deref(),
            Some("line one line two")
        );
    }

    #[test]
    fn generic_body_probes_common_keys_and_never_dumps_a_structure() {
        assert_eq!(
            result_body("Whatever", &json!({"content": "hi"})).as_deref(),
            Some("hi")
        );
        assert_eq!(
            result_body("Whatever", &json!("bare string")).as_deref(),
            Some("bare string")
        );
        // A structure that matched none of the probes has NO human-readable
        // body. Serializing it shipped a tool's whole internal payload to
        // every client as user-visible text.
        assert!(result_body("Whatever", &json!({"odd": 1})).is_none());
        // A scalar still renders — it is its own text.
        assert_eq!(result_body("Whatever", &json!(7)).as_deref(), Some("7"));
        assert_eq!(
            result_body("Whatever", &json!(true)).as_deref(),
            Some("true")
        );
        // An MCP tool's `data` IS its content-block array; the text blocks are
        // the body.
        assert_eq!(
            result_body(
                "mcp__srv__thing",
                &json!([{"type": "text", "text": "one"}, {"type": "text", "text": "two"}])
            )
            .as_deref(),
            Some("one\ntwo")
        );
        assert!(result_body("Whatever", &json!(null)).is_none());
    }

    #[test]
    fn an_edit_result_body_is_the_diff_not_the_whole_pre_edit_file() {
        // The literal `data` from `tools/file/src/edit.rs` — no key of which
        // `generic_body` probes, so the whole object (INCLUDING the entire
        // pre-edit `originalFile`) used to become the user-visible body, and
        // compact JSON being one line it shipped `collapsed: false` too.
        let result = json!({
            "filePath": "/tmp/x.rs",
            "oldString": "fn a() {}\n",
            "newString": "fn b() {}\n",
            "originalFile": "fn a() {}\nline2\nline3\nline4\nline5\n",
            "structuredPatch": "-fn a() {}\n+fn b() {}\n",
            "userModified": false,
            "replaceAll": false,
        });
        for tool in ["Edit", "MultiEdit", "Write", "NotebookEdit"] {
            let body = result_body(tool, &result);
            assert!(
                body.is_none(),
                "{tool} body should be the diff, got {body:?}"
            );
        }
        // An error still surfaces — the diff cannot say what went wrong.
        let failed = json!({"error": "String not found in file"});
        assert_eq!(
            result_body("Edit", &failed).as_deref(),
            Some("String not found in file")
        );
    }

    #[test]
    fn a_resumed_transcript_keeps_a_bash_or_read_result() {
        // A persisted transcript stores the tool's MODEL-FACING STRING, never
        // the `ToolCallResult.data` object: Bash's is
        // `bash_model_content(stdout, stderr, …)` and Read's is the cat -n
        // render. Indexing those as objects lost the whole result — Bash
        // headlined "(No content)" and rendered an empty body.
        let bash = json!("compiling…\nwarning: unused\ndone");
        assert_eq!(
            result_headline("Bash", None, &bash, false).as_deref(),
            Some("compiling…")
        );
        assert_eq!(
            result_body("Bash", &bash).as_deref(),
            Some("compiling…\nwarning: unused\ndone")
        );
        let empty = json!("");
        assert_eq!(
            result_headline("Bash", None, &empty, false).as_deref(),
            Some("(No content)")
        );
        assert!(result_body("Bash", &empty).is_none());

        let read = json!("     1\tone\n     2\ttwo\n     3\tthree");
        assert_eq!(
            result_headline("Read", None, &read, false).as_deref(),
            Some("Read 3 lines")
        );
        assert_eq!(
            result_body("Read", &read).as_deref(),
            Some("     1\tone\n     2\ttwo\n     3\tthree")
        );

        // The edit family is unaffected: it reads the retained CALL INPUT,
        // which a resumed transcript does pair with the result.
        let input = json!({
            "file_path": "/tmp/x.rs",
            "old_string": "a\nb\n",
            "new_string": "a\nB\nc\n",
        });
        assert_eq!(
            result_headline(
                "Edit",
                Some(&input),
                &json!("The file has been updated."),
                false
            )
            .as_deref(),
            Some("Added 2 lines, removed 1 line")
        );
    }

    #[test]
    fn clamp_body_reports_the_pre_clamp_line_count() {
        let short = clamp_body("a\nb\nc");
        assert_eq!(short.total_lines, 3);
        assert!(!short.truncated);
        assert!(!short.collapsed, "short bodies render inline");

        let long: String = (0..(MAX_BODY_LINES + 25))
            .map(|n| format!("line{n}\n"))
            .collect();
        let clamped = clamp_body(&long);
        assert_eq!(clamped.total_lines, MAX_BODY_LINES + 25);
        assert!(clamped.truncated);
        assert!(clamped.collapsed);
        assert_eq!(clamped.text.lines().count(), MAX_BODY_LINES);
    }

    #[test]
    fn clamp_body_cuts_on_a_char_boundary() {
        // A body of multi-byte characters must not be sliced mid-codepoint.
        let body = "中".repeat(MAX_BODY_BYTES);
        let clamped = clamp_body(&body);
        assert!(clamped.truncated);
        assert!(clamped.text.len() <= MAX_BODY_BYTES);
        // Valid UTF-8 by construction — this would have panicked on a bad cut.
        assert!(clamped.text.chars().all(|c| c == '中'));
    }

    #[test]
    fn collapse_threshold_tracks_the_inline_budget() {
        let exactly: String = (0..INLINE_BODY_BUDGET).map(|_| "x\n").collect();
        assert!(!clamp_body(&exactly).collapsed);
        let one_more: String = (0..=INLINE_BODY_BUDGET).map(|_| "x\n").collect();
        assert!(clamp_body(&one_more).collapsed);
    }
}
