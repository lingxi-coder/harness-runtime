//! Transport-neutral derivation of how one tool call is presented.
//!
//! Everything here is plain data computed from `(tool_name, input_json,
//! result_json)`. It has no terminal dependency and no styling: the terminal
//! renders it through `crate::presentation::render`, and `client::adapter` lowers it onto the
//! wire for the iOS, Android, and Electron clients. Deriving it once is the
//! point — before this module each surface built its own header text and
//! result summary, and four implementations had already drifted apart.
//!
//! This component is shared by terminal and native clients and must not
//! depend on terminal I/O or a particular client protocol.
//!
//! Three pieces:
//!
//! - [`header`] — the parameterized call header (`Update(src/host.rs)`).
//! - [`result`] — the `⎿` result headline and the expandable body.
//! - [`plan`] — the model-managed todo checklist pinned above the composer.

pub mod header;
pub mod plan;
pub mod result;

pub use header::{
    activity_label, tool_header, tool_header_with_result, tool_icon, ToolHeader, ToolIcon,
    ToolSubLine, ToolVerb,
};
pub use plan::{PlanTask, PlanTaskState};
pub use result::{added_removed_header, result_body, result_headline, result_is_error};

/// Derive the diff-source fields the `UserToolResult` renderer needs from a
/// tool-call input (mirrors the iocraft `tui::streaming::diff_inputs_for`,
/// which stays in place until the iocraft backend is deleted).
#[must_use]
pub fn diff_inputs_for(
    tool: &str,
    input: &serde_json::Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let str_key = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match tool {
        "Edit" => (
            str_key("old_string"),
            str_key("new_string"),
            str_key("file_path"),
        ),
        "Write" => (None, str_key("content"), str_key("file_path")),
        // MultiEdit (`edits[]`) and NotebookEdit (cell-shaped) carry no single
        // old→new pair — surface only the path so the header renders without a
        // (wrong) single-hunk diff.
        "MultiEdit" | "NotebookEdit" => (None, None, str_key("file_path")),
        _ => (None, None, None),
    }
}
