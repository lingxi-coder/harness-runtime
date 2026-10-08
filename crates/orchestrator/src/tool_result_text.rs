//! Shared Claude Code coercion for live and resumed non-text tool results.

use lingxi_core::types::utf16_json::Utf16JsonProjection;

pub(crate) fn root_text(text: &str) -> Utf16JsonProjection {
    text.into()
}

pub(crate) fn normalized_tool_result_projection(
    value: &Utf16JsonProjection,
) -> Utf16JsonProjection {
    // JSON.stringify emits lone units as escapes. Keep the source object until
    // that operation, then truncate its resulting string at a code-point edge.
    let text = value.to_json_string().expect("valid source tool result");
    let mut units = 0;
    let end = text.char_indices().find_map(|(index, character)| {
        units += character.len_utf16();
        (units > 50_000).then_some(index)
    });
    let Some(end) = end else {
        return text.into();
    };
    let omitted = text[end..].chars().count();
    format!(
        "{}\n[{omitted} more character{} of this tool result not shown]",
        &text[..end],
        if omitted == 1 { "" } else { "s" }
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stringify_preserves_lone_units_and_javascript_numeric_key_order() {
        let value = Utf16JsonProjection::parse(r#"{"10":"x","2":"\ud800","body":"Ω"}"#).unwrap();
        let text = normalized_tool_result_projection(&value);
        assert_eq!(
            text.string_at(""),
            Some(r#"{"2":"\ud800","10":"x","body":"Ω"}"#)
        );
    }

    #[test]
    fn truncation_keeps_a_surrogate_pair_whole_and_counts_omitted_code_points() {
        let value: Utf16JsonProjection = format!("{}😀z", "a".repeat(49_998)).into();
        let result = normalized_tool_result_projection(&value);
        let expected = format!(
            "\"{}\n[3 more characters of this tool result not shown]",
            "a".repeat(49_998)
        );
        assert_eq!(result.string_at(""), Some(expected.as_str()));
    }

    #[test]
    fn cold_resume_object_matches_native_293_raw_content_bytes() {
        let row = Utf16JsonProjection::parse(include_str!(
            "../../../docs/parity/current-alignment/fixtures/tool-result-json-resume/native-baseline/cold-resume-object/resume-input-tool-result-session-row.raw.jsonl"
        ))
        .unwrap();
        let content = row.subprojection("/message/content/0/content").unwrap();
        let actual = normalized_tool_result_projection(&content)
            .to_json_string()
            .unwrap();
        let expected = include_bytes!(
            "../../../docs/parity/current-alignment/fixtures/tool-result-json-resume/native-baseline/cold-resume-object/model-tool-result-content.raw.json"
        );
        assert_eq!(actual.as_bytes(), expected);
    }
}
