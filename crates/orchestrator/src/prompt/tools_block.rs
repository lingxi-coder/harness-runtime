//! `<tools>...</tools>` formatter — emits the available-tool-name list
//! (one per line). Full tool schemas are wire-side via the API
//! `tools: [...]` array; the prompt only carries names so the model
//! can call by name.
//!
//! Names are sorted alphabetically inside the formatter so callers
//! can pass an unsorted `Vec` and still get byte-stable output.
#![forbid(unsafe_code)]

/// Format the `<tools>...</tools>` block from a slice of tool names.
/// When `names` is empty, returns the EMPTY STRING — caller MUST
/// elide the section.
#[must_use]
pub fn format(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut s = String::with_capacity(64 + sorted.len() * 24);
    s.push_str("<tools>\n");
    for n in sorted {
        s.push_str("- ");
        s.push_str(n);
        s.push('\n');
    }
    s.push_str("</tools>\n");
    s
}
