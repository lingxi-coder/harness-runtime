//! Shared native Zod diagnostics for main and nested tool dispatch.
//!
//! JSON rendering and issue grouping are executed-oracle locked to Claude Code
//! 2.1.270. The flat-schema collector keeps issue paths and strict-object keys
//! as exact UTF-16 projections; richer schemas stay with the orchestrator's
//! existing collector.
#![allow(missing_docs)]
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

#[derive(Debug)]
pub struct NativeSchemaError {
    /// Human-readable formatted issue text as an exact root-string projection.
    pub display: Utf16JsonProjection,
    /// `JSON.stringify(issues, null, 2)` as a root-string projection. Keeping
    /// this source-native matters because strict-object issues include keys
    /// copied from the original input, which may contain unpaired UTF-16.
    pub raw: Utf16JsonProjection,
}

pub fn format_issues(name: &str, issues: &[Utf16JsonProjection]) -> Utf16JsonProjection {
    let mut missing = Vec::<Vec<u16>>::new();
    let mut unexpected = Vec::<Vec<u16>>::new();
    let mut types = Vec::<Vec<u16>>::new();
    for issue in issues {
        let path = issue_path(issue);
        let message = source_property_string(issue, "message").unwrap_or_default();
        match source_property_string(issue, "code").as_deref() {
            Some("invalid_type") if message.contains("received undefined") => {
                let mut rendered = "The required parameter `"
                    .encode_utf16()
                    .collect::<Vec<_>>();
                rendered.extend(path);
                rendered.extend("` is missing".encode_utf16());
                missing.push(rendered);
            }
            Some("invalid_type") => {
                let received = message
                    .split_once("received ")
                    .map(|(_, suffix)| {
                        suffix
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .collect::<String>()
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unknown".into());
                let expected = source_property_string(issue, "expected")
                    .unwrap_or_else(|| "undefined".to_owned());
                let mut rendered = "The parameter `".encode_utf16().collect::<Vec<_>>();
                rendered.extend(path);
                rendered.extend("` type is expected as `".encode_utf16());
                rendered.extend(expected.encode_utf16());
                rendered.extend("` but provided as `".encode_utf16());
                rendered.extend(received.encode_utf16());
                rendered.extend("`".encode_utf16());
                types.push(rendered);
            }
            Some("unrecognized_keys") => {
                if let Some(keys) = issue.at_pointer("/keys").and_then(JsonValueView::array_len) {
                    for index in 0..keys {
                        if let Some(key) = issue.string_units(&format!("/keys/{index}")) {
                            let mut rendered = "An unexpected parameter `"
                                .encode_utf16()
                                .collect::<Vec<_>>();
                            rendered.extend(key);
                            rendered.extend("` was provided".encode_utf16());
                            unexpected.push(rendered);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let messages: Vec<_> = missing.into_iter().chain(unexpected).chain(types).collect();
    if messages.is_empty() {
        let array = Utf16JsonProjection::array(issues.to_vec())
            .expect("native issue projections are valid JSON");
        string_projection(
            array
                .to_pretty_json_string()
                .expect("native issue projections stringify as JSON"),
        )
    } else {
        let mut rendered = format!(
            "{name} failed due to the following {}:\n",
            if messages.len() == 1 {
                "issue"
            } else {
                "issues"
            }
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        for (index, message) in messages.iter().enumerate() {
            if index != 0 {
                rendered.push(b'\n' as u16);
            }
            rendered.extend(message);
        }
        string_projection_units(rendered)
    }
}

/// Return the Native fallback diagnostic `JSON.stringify(issues, null, 2)` as
/// an exact root-string projection. The issue list and every property key stay
/// source projections throughout formatting.
pub fn format_raw_issues(issues: &[Utf16JsonProjection]) -> Utf16JsonProjection {
    let array = Utf16JsonProjection::array(issues.to_vec())
        .expect("native issue projections are valid JSON");
    string_projection(
        array
            .to_pretty_json_string()
            .expect("native issue projections stringify as JSON"),
    )
}

fn issue_path(issue: &Utf16JsonProjection) -> Vec<u16> {
    let mut result = Vec::new();
    let Some(length) = issue.at_pointer("/path").and_then(JsonValueView::array_len) else {
        return result;
    };
    for index in 0..length {
        let pointer = format!("/path/{index}");
        if let Some(key) = issue.string_units(&pointer) {
            if index != 0 {
                result.push(b'.' as u16);
            }
            result.extend(key);
        } else if let Some(number) = issue
            .at_pointer(&pointer)
            .and_then(JsonValueView::as_number)
        {
            result.extend(format!("[{number}]").encode_utf16());
        }
    }
    result
}

fn source_property_string(issue: &Utf16JsonProjection, property: &str) -> Option<String> {
    let units = issue.string_units(&format!("/{property}"))?;
    String::from_utf16(&units).ok()
}

fn string_projection(value: String) -> Utf16JsonProjection {
    string_projection_units(value.encode_utf16().collect())
}

fn string_projection_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display string matches exact UTF-16 units")
}

fn object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("native schema issue is valid JSON")
}

fn array(values: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::array(values).expect("native schema issue array is valid JSON")
}

fn source_string(value: &str) -> Utf16JsonProjection {
    string_projection(value.to_owned())
}

fn source_string_units(units: Vec<u16>) -> Utf16JsonProjection {
    string_projection_units(units)
}

/// Zod object parsing ignores the unsafe prototype key rather than forwarding it.
pub fn normalize_flat_input(input: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let entries = input.object_entries()?;
    let proto_key: Vec<u16> = "__proto__".encode_utf16().collect();
    if !entries.iter().any(|entry| entry.code_units == proto_key) {
        return None;
    }
    let mut fields = Vec::with_capacity(entries.len().saturating_sub(1));
    for entry in entries {
        if entry.code_units == proto_key {
            continue;
        }
        let key = entry.code_units.to_vec();
        fields.push((key.clone(), input.subprojection_property_units(&key).ok()?));
    }
    Utf16JsonProjection::object_units(fields).ok()
}

/// Return None for schema vocabulary outside this narrowly shared collector.
/// Issue objects remain source projections so strict-object keys copied from
/// arbitrary input never pass through `serde_json::Value`.
pub fn collect_flat_issues(
    schema: &Utf16JsonProjection,
    input: &Utf16JsonProjection,
) -> Option<Vec<Utf16JsonProjection>> {
    let schema_root = schema.at_pointer("")?;
    let schema_entries = schema_root.object_entries()?;
    if member(schema_root, "type").and_then(JsonValueView::as_str) != Some("object")
        || member(schema_root, "additionalProperties").and_then(JsonValueView::as_bool)
            != Some(false)
    {
        return None;
    }
    if schema_entries.iter().any(|entry| {
        !matches!(
            entry.display_key,
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "$schema"
                | "description"
                | "title"
        )
    }) {
        return None;
    }
    let properties_view = member(schema_root, "properties")?;
    let mut properties = properties_view.object_entries()?;
    sort_object_entries_like_javascript(&mut properties);
    if properties.iter().any(|property| {
        !matches!(
            member(property.value, "type").and_then(JsonValueView::as_str),
            Some("string" | "number" | "boolean")
        ) || property.value.object_entries().is_none_or(|fields| {
            fields
                .iter()
                .any(|field| !matches!(field.display_key, "type" | "description" | "title"))
        })
    }) {
        return None;
    }

    let mut issues = Vec::new();
    let input_root = input.at_pointer("")?;
    let Some(object_entries) = input_root.object_entries() else {
        issues.push(type_issue("object", Some(input_root), Vec::new()));
        return Some(issues);
    };

    let required = member(schema_root, "required").and_then(JsonValueView::array_len);
    let required_view = member(schema_root, "required");
    for property in &properties {
        let required_here = required.is_some_and(|length| {
            (0..length).any(|index| {
                required_view
                    .and_then(|items| items.get_index(index))
                    .and_then(JsonValueView::code_units)
                    .is_some_and(|name| name == property.code_units)
            })
        });
        let value = input_root.get_property_units(property.code_units);
        if value.is_none() && !required_here {
            continue;
        }
        let expected = member(property.value, "type")
            .and_then(JsonValueView::as_str)
            .expect("flat schema property was validated above");
        if source_value_type(value) != expected {
            issues.push(type_issue(
                expected,
                value,
                vec![property.code_units.to_vec()],
            ));
        }
    }

    let proto_key: Vec<u16> = "__proto__".encode_utf16().collect();
    let mut keys: Vec<Vec<u16>> = object_entries
        .iter()
        .filter(|input_entry| {
            input_entry.code_units != proto_key
                && !properties
                    .iter()
                    .any(|property| property.code_units == input_entry.code_units)
        })
        .map(|entry| entry.code_units.to_vec())
        .collect();
    sort_key_units_like_javascript(&mut keys);
    if !keys.is_empty() {
        let mut message = format!(
            "Unrecognized key{}: ",
            if keys.len() == 1 { "" } else { "s" }
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        for (index, key) in keys.iter().enumerate() {
            if index != 0 {
                message.extend(", ".encode_utf16());
            }
            message.push(b'"' as u16);
            message.extend(key);
            message.push(b'"' as u16);
        }
        issues.push(object(vec![
            ("code".into(), source_string("unrecognized_keys")),
            (
                "keys".into(),
                array(keys.into_iter().map(source_string_units).collect()),
            ),
            ("path".into(), array(Vec::new())),
            ("message".into(), source_string_units(message)),
        ]));
    }
    Some(issues)
}

pub fn validate_flat_input(
    name: &str,
    schema: &Utf16JsonProjection,
    input: &Utf16JsonProjection,
) -> Option<Result<(), NativeSchemaError>> {
    let issues = collect_flat_issues(schema, input)?;
    Some(if issues.is_empty() {
        Ok(())
    } else {
        Err(NativeSchemaError {
            display: format_issues(name, &issues),
            raw: format_raw_issues(&issues),
        })
    })
}

fn member<'a>(value: JsonValueView<'a>, key: &str) -> Option<JsonValueView<'a>> {
    value.get_property_units(&key.encode_utf16().collect::<Vec<_>>())
}

fn source_value_type(value: Option<JsonValueView<'_>>) -> &'static str {
    let Some(value) = value else {
        return "undefined";
    };
    if value.is_null() {
        "null"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if let Some(number) = value.as_number() {
        if number.is_nan() {
            "NaN"
        } else if number == f64::NEG_INFINITY {
            "-Infinity"
        } else if number == f64::INFINITY {
            "Infinity"
        } else {
            "number"
        }
    } else if value.code_units().is_some() {
        "string"
    } else if value.array_len().is_some() {
        "array"
    } else {
        "object"
    }
}

fn type_issue(
    expected: &str,
    value: Option<JsonValueView<'_>>,
    path: Vec<Vec<u16>>,
) -> Utf16JsonProjection {
    let received = source_value_type(value);
    let mut fields = vec![
        ("expected".into(), source_string(expected)),
        ("code".into(), source_string("invalid_type")),
    ];
    if matches!(received, "NaN" | "Infinity" | "-Infinity") {
        fields.push(("received".into(), source_string(received)));
    }
    fields.extend([
        (
            "path".into(),
            array(path.into_iter().map(source_string_units).collect()),
        ),
        (
            "message".into(),
            source_string(&format!(
                "Invalid input: expected {expected}, received {}",
                received
            )),
        ),
    ]);
    object(fields)
}

fn js_array_index(key: &str) -> Option<u32> {
    let number = key.parse::<u32>().ok()?;
    (number != u32::MAX && number.to_string() == key).then_some(number)
}

fn sort_object_entries_like_javascript(
    entries: &mut [lingxi_core::types::utf16_json::JsonObjectEntryView<'_>],
) {
    entries.sort_by(|left, right| {
        let left_index = String::from_utf16(left.code_units)
            .ok()
            .and_then(|key| js_array_index(&key));
        let right_index = String::from_utf16(right.code_units)
            .ok()
            .and_then(|key| js_array_index(&key));
        match (left_index, right_index) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

fn sort_key_units_like_javascript(keys: &mut [Vec<u16>]) {
    keys.sort_by(|left, right| {
        let left_index = String::from_utf16(left)
            .ok()
            .and_then(|key| js_array_index(&key));
        let right_index = String::from_utf16(right)
            .ok()
            .and_then(|key| js_array_index(&key));
        match (left_index, right_index) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

#[cfg(test)]
mod source_projection_tests {
    use super::*;

    fn parse(input: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(input).expect("valid JSON fixture")
    }

    #[test]
    fn flat_validation_reads_source_schema_and_input_without_value_ingress() {
        let schema = parse(
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
        );
        let input = parse(r#"{"path":"src/main.rs"}"#);

        assert!(matches!(
            validate_flat_input("Read", &schema, &input),
            Some(Ok(()))
        ));
        assert!(matches!(
            validate_flat_input("Read", &schema, &parse(r#"{"path":42}"#)),
            Some(Err(_))
        ));
    }

    #[test]
    fn unsafe_prototype_field_is_removed_without_losing_lone_surrogate_source() {
        let schema = parse(
            r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
        );
        let mut input_json: Vec<u16> = r#"{"text":""#.encode_utf16().collect();
        input_json.push(0xd800);
        input_json.extend(r#"","__proto__":true}"#.encode_utf16());
        let input = Utf16JsonProjection::parse_utf16_json(&input_json)
            .expect("literal lone surrogate is valid inside a JSON string");

        let normalized = normalize_flat_input(&input).expect("prototype key was present");

        assert_eq!(normalized.string_units("/text"), Some(vec![0xd800]));
        assert!(!normalized.contains_property("__proto__"));
        assert!(matches!(
            validate_flat_input("Read", &schema, &normalized),
            Some(Ok(()))
        ));
    }

    #[test]
    fn strict_object_error_keeps_lone_surrogate_keys_in_source_diagnostics() {
        let schema = parse(
            r#"{"type":"object","properties":{"name":{"type":"string"}},"additionalProperties":false}"#,
        );
        let input = parse(r#"{"\ud800":true}"#);
        let Some(Err(error)) = validate_flat_input("Workflow", &schema, &input) else {
            panic!("unexpected keys must be reported");
        };

        let display = error
            .display
            .string_units("")
            .expect("display is root string");
        assert!(display.contains(&0xd800));
        let raw = String::from_utf16(&error.raw.string_units("").expect("raw is root string"))
            .expect("JSON-stringified issues are valid Unicode text");
        assert!(raw.contains("\\ud800"));
    }
}
