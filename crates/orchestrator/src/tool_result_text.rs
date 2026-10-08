//! Shared Claude Code 2.1.286 coercion for live and resumed non-text tool results.

use serde_json::Value;

pub(crate) fn normalized_tool_result_text(value: &Value) -> String {
    // Claude Code 2.1.286 Fgn/$gn: JSON.stringify, then a surrogate-safe
    // 50,000 UTF-16-unit prefix and the number of omitted Unicode characters.
    let text = tool_api::native_schema::js_json(value, false);
    let mut units = 0;
    let end = text.char_indices().find_map(|(index, character)| {
        units += character.len_utf16();
        (units > 50_000).then_some(index)
    });
    let Some(end) = end else {
        return text;
    };
    let omitted = text[end..].chars().count();
    format!(
        "{}\n[{omitted} more character{} of this tool result not shown]",
        &text[..end],
        if omitted == 1 { "" } else { "s" }
    )
}
