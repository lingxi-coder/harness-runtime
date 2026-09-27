//! Shared text formatter used by `/mcp`, `/hooks`, and `/agents`.
//!
//! Locked layout (M5-11 T0 step 2 L6/L7/L8 + M6-07 empty-state literal):
//!   Non-empty: `"<Label> ({count}):\n  <row1>\n  <row2>\n…"`
//!   Empty:     `"<empty_state>\n"`
//! with each row pre-formatted by the caller as a single line (no trailing
//! newline). The render adds the two leading spaces + trailing newline per
//! row, and the trailing newline at end of the buffer.

/// Render a labelled list with the locked layout, or the supplied
/// `empty_state` literal when `rows` is empty.
///
/// Non-empty output: `"<Label> ({count}):\n  <row1>\n  <row2>\n…"`
/// Empty output:     `"<empty_state>\n"`
#[must_use]
pub fn render_list(label: &str, rows: Vec<String>, empty_state: &str) -> String {
    if rows.is_empty() {
        return format!("{empty_state}\n");
    }
    let count = rows.len();
    let mut out = format!("{label} ({count}):\n");
    for row in rows {
        out.push_str("  ");
        out.push_str(&row);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_renders_empty_state_literal() {
        let s = render_list("MCP servers", vec![], "No MCP servers configured");
        assert_eq!(s, "No MCP servers configured\n");
    }

    #[test]
    fn multi_row_renders_with_leading_two_spaces() {
        let s = render_list(
            "Hooks",
            vec!["fmt PostToolUse 60000ms".into(), "lint Stop 30000ms".into()],
            "No hooks configured",
        );
        assert_eq!(
            s,
            "Hooks (2):\n  fmt PostToolUse 60000ms\n  lint Stop 30000ms\n"
        );
    }

    #[test]
    fn empty_state_is_used_for_zero_rows_only() {
        // Single row stays in the labelled-list shape.
        let s = render_list(
            "Agents",
            vec!["reviewer  review code".into()],
            "No subagents configured",
        );
        assert_eq!(s, "Agents (1):\n  reviewer  review code\n");
    }
}
