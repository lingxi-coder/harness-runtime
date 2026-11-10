//! Native 2.1.322 `rFr` staged-call schema. Issue order and rendering are
//! compared with the hash-pinned process fixture before filesystem work.
use super::{error, object, text, Json};
use lingxi_core::types::utf16_json::JsonValueView;

fn received(value: Option<JsonValueView<'_>>) -> &'static str {
    match value {
        None => "undefined",
        Some(value) if value.is_null() => "null",
        Some(value) if value.code_units().is_some() => "string",
        Some(value) if value.as_bool().is_some() => "boolean",
        Some(value) if value.array_len().is_some() => "array",
        Some(value) if value.as_number().is_some() => "number",
        Some(_) => "object",
    }
}

fn typed(
    issues: &mut Vec<Json>,
    value: Option<JsonValueView<'_>>,
    expected: &str,
    path: Vec<Json>,
) -> bool {
    let valid = match expected {
        "record" | "object" => value.is_some_and(|value| value.object_entries().is_some()),
        "number" => value
            .and_then(JsonValueView::as_number)
            .is_some_and(f64::is_finite),
        _ => received(value) == expected,
    };
    if !valid {
        let kind = received(value);
        let mut fields = vec![("expected", text(expected)), ("code", text("invalid_type"))];
        let kind = value
            .and_then(JsonValueView::as_number)
            .filter(|number| !number.is_finite())
            .map_or(
                kind,
                |number| if number.is_nan() { "NaN" } else { "Infinity" },
            );
        if matches!(kind, "NaN" | "Infinity") {
            fields.push(("received", text(kind)));
        }
        fields.extend([
            ("path", Json::array(path).unwrap()),
            (
                "message",
                text(format!(
                    "Invalid input: expected {expected}, received {kind}"
                )),
            ),
        ]);
        issues.push(object(fields));
    }
    valid
}

pub(super) fn refusal(request: &Json) -> Option<Json> {
    let mut issues = Vec::new();
    for (key, expected) in [
        ("arguments", "record"),
        ("expires_at", "string"),
        ("timeout_ms", "number"),
        ("result_not_for_model", "boolean"),
        ("check_permissions", "boolean"),
    ] {
        if let Some(value) = request.get_property(key) {
            typed(&mut issues, Some(value), expected, vec![text(key)]);
        }
    }
    for key in ["input_files", "output_files"] {
        let Some(files) = request.get_property(key) else {
            continue;
        };
        if !typed(&mut issues, Some(files), "array", vec![text(key)]) {
            continue;
        }
        let count = files.array_len().unwrap();
        for index in 0..count {
            let value = files.get_index(index);
            let path = vec![text(key), Json::parse(&index.to_string()).unwrap()];
            if !typed(&mut issues, value, "object", path.clone()) {
                continue;
            }
            let value = value.unwrap();
            for field in ["name", "lane_path", "if_match"] {
                if field == "if_match"
                    && (key == "input_files" || value.get_property(field).is_none())
                {
                    continue;
                }
                let field_value = value.get_property(field);
                let mut path = path.clone();
                path.push(text(field));
                if !typed(&mut issues, field_value, "string", path.clone()) {
                    continue;
                }
                let units = field_value.unwrap().code_units().unwrap();
                if field == "name"
                    && (units.is_empty()
                        || units.len() > 64
                        || !units
                            .iter()
                            .all(|unit| matches!(*unit, 45 | 48..=57 | 65..=90 | 95 | 97..=122)))
                {
                    issues.push(object(vec![
                        ("origin", text("string")),
                        ("code", text("invalid_format")),
                        ("format", text("regex")),
                        ("pattern", text("/^[A-Za-z0-9_-]{1,64}$/")),
                        ("path", Json::array(path).unwrap()),
                        (
                            "message",
                            text("Invalid string: must match pattern /^[A-Za-z0-9_-]{1,64}$/"),
                        ),
                    ]));
                } else if field == "if_match" && units.is_empty() {
                    issues.push(object(vec![
                        ("origin", text("string")),
                        ("code", text("too_small")),
                        ("minimum", Json::parse("1").unwrap()),
                        ("inclusive", Json::parse("true").unwrap()),
                        ("path", Json::array(path).unwrap()),
                        (
                            "message",
                            text("Too small: expected string to have >=1 characters"),
                        ),
                    ]));
                }
            }
        }
        if count > 64 {
            issues.push(object(vec![
                ("origin", text("array")),
                ("code", text("too_big")),
                ("maximum", Json::parse("64").unwrap()),
                ("inclusive", Json::parse("true").unwrap()),
                ("path", Json::array(vec![text(key)]).unwrap()),
                (
                    "message",
                    text("Too big: expected array to have <=64 items"),
                ),
            ]));
        }
    }
    if issues.is_empty() {
        return None;
    }
    let rendered = Json::array(issues)
        .unwrap()
        .to_pretty_json_string()
        .unwrap();
    Some(error(
        "tool_error",
        format!("invalid request: {rendered}"),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_322_staged_request_schema_diagnostics_are_exact() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/mcp_sdk_staging_schema_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let request = Json::from_serde_value(&case["request"]).unwrap();
            let actual = refusal(&request).expect("schema fixture refuses before the tool runs");
            let expected = Json::from_serde_value(&case["response"]["response"]).unwrap();
            assert_eq!(
                actual.string_at("/staging/detail"),
                expected.string_at("/staging/detail"),
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&actual.to_json_string().unwrap())
                    .unwrap(),
                case["response"]["response"],
                "{}",
                case["name"]
            );
        }
    }
}
