//! Tool-output truncation — shared helpers for tools that emit user-visible
//! output.
//!
//! [`MAX_TOOL_OUTPUT_LENGTH`] (`= 30_000` chars, NOT bytes — UTF-8 safe) is the
//! shell-family output cap (`BASH_MAX_OUTPUT_LENGTH` default `mFi`/`vGt()` in
//! claude-code 2.1.206). [`truncate_shell_output`] is the canonical
//! shell-command truncator — a 1:1 port of claude-code's shared `Qyu()`
//! (`BashTool/utils.ts:156-158`): keep the first `max` chars verbatim, then
//! append `"\n\n... [N lines truncated] ..."`, where `N` is the number of
//! newlines in the truncated tail plus one. Bash / REPL / PowerShell all use
//! this form.
//!
//! (Non-shell tools truncate with their own byte-locked suffixes: `FileRead` /
//! `Edit` use `"... [N lines truncated] ..."` too; `WebFetch` uses
//! `"[Content truncated due to length...]"` at 1 MB; MCP tool results use
//! `"[OUTPUT TRUNCATED - exceeded N token limit]..."`. There is NO generic
//! `"[Output truncated due to length]"` suffix in the 2.1.206 binary — an
//! earlier port copied that from stale leaked TS; it has been removed.)

/// Maximum char count of shell-family tool output before truncation kicks in
/// (claude-code `BASH_MAX_OUTPUT_LENGTH` default). UTF-8 char count, not bytes.
pub const MAX_TOOL_OUTPUT_LENGTH: usize = 30_000;

/// Parity anchor for the suffix [`truncate_shell_output`] appends — the
/// claude-code `Qyu()` form with the line count as `{N}` (`BashTool/utils.ts:156-158`).
/// The runtime value substitutes the real count for `{N}`.
pub const SHELL_TRUNCATION_SUFFIX_TEMPLATE: &str = "\n\n... [{N} lines truncated] ...";

/// Canonical shell-command output truncation — port of claude-code `Qyu()`
/// (`BashTool/utils.ts:156-158`). When `content` exceeds `max` chars, keep the
/// first `max` chars verbatim and append `"\n\n... [N lines truncated] ..."`,
/// where `N` is the count of `'\n'` in the truncated tail plus one. When it
/// fits, `content` is returned unchanged.
///
/// Returns `(out, did_truncate)`. Char-based slicing keeps UTF-8 codepoints
/// intact.
#[must_use]
pub fn truncate_shell_output(content: String, max: usize) -> (String, bool) {
    if content.chars().count() <= max {
        return (content, false);
    }
    let head: String = content.chars().take(max).collect();
    // `N` = newlines in everything after the kept head (the truncated tail)
    // plus one, matching `countCharInString(content, '\n', max) + 1`.
    let remaining_lines = content.chars().skip(max).filter(|&c| c == '\n').count() + 1;
    let truncated = format!("{head}\n\n... [{remaining_lines} lines truncated] ...");
    (truncated, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_tool_output_length_is_30_000() {
        assert_eq!(MAX_TOOL_OUTPUT_LENGTH, 30_000);
    }

    #[test]
    fn short_string_passes_through() {
        let (out, trunc) = truncate_shell_output("hello".to_string(), 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
    }

    #[test]
    fn exact_length_passes_through() {
        let s = "a".repeat(100);
        let (out, trunc) = truncate_shell_output(s.clone(), 100);
        assert_eq!(out, s);
        assert!(!trunc);
    }

    #[test]
    fn over_limit_appends_lines_truncated_suffix() {
        // 100 chars of "a" then 3 newlines then more — tail after max=50 has
        // its newlines counted (+1).
        let s = format!("{}{}", "a".repeat(50), "\n\nbcd");
        let (out, trunc) = truncate_shell_output(s, 50);
        assert!(trunc);
        // tail = "\n\nbcd" → 2 newlines + 1 = 3.
        assert_eq!(
            out,
            format!("{}\n\n... [3 lines truncated] ...", "a".repeat(50))
        );
    }

    #[test]
    fn single_line_tail_reports_one_line() {
        let s = "a".repeat(60); // no newlines in the tail
        let (out, trunc) = truncate_shell_output(s, 50);
        assert!(trunc);
        // tail = 10 "a"s, 0 newlines + 1 = 1.
        assert_eq!(
            out,
            format!("{}\n\n... [1 lines truncated] ...", "a".repeat(50))
        );
    }

    #[test]
    fn keeps_utf8_codepoints_intact() {
        let s: String = "🎉".repeat(100);
        let (out, trunc) = truncate_shell_output(s, 40);
        assert!(trunc);
        let head: String = "🎉".repeat(40);
        assert_eq!(out, format!("{head}\n\n... [1 lines truncated] ..."));
    }

    #[test]
    fn truncates_at_30k_default() {
        let s = "x".repeat(40_000);
        let (out, trunc) = truncate_shell_output(s, MAX_TOOL_OUTPUT_LENGTH);
        assert!(trunc);
        assert!(out.starts_with(&"x".repeat(MAX_TOOL_OUTPUT_LENGTH)));
        assert!(out.ends_with("lines truncated] ..."));
    }
}
