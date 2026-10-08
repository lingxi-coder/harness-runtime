//! Source-native implementations of the local Zod output parsers whose
//! schemas are deliberately small object shapes.
//!
//! This is not a JSON Schema validator. Each tool still owns the exact shape
//! and nested parsing behavior corresponding to its Native `safeParse` call;
//! this module only provides borrowed type checks and Zod's default object
//! stripping operation without lowering arbitrary results through
//! `serde_json::Value`.

use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

use crate::tool_trait::ToolError;

#[derive(Clone, Copy)]
pub enum OutputFieldType {
    String,
    Number,
    Boolean,
    Array,
    Object,
    ObjectShape(&'static [OutputField]),
    StringOrNull,
    False,
}

#[derive(Clone, Copy)]
pub struct OutputField {
    pub name: &'static str,
    pub kind: OutputFieldType,
    pub required: bool,
}

#[derive(Clone, Copy)]
pub enum UnknownOutputKeys {
    /// Default Zod objects return only known properties.
    Strip,
    /// Non-strict passthrough objects retain unrecognized properties.
    Passthrough,
}

pub fn parse_object(
    input: &Utf16JsonProjection,
    tool_name: &str,
    fields: &[OutputField],
    unknown_keys: UnknownOutputKeys,
) -> Result<Utf16JsonProjection, ToolError> {
    let mut issues = Vec::new();
    let parsed = parse_object_fields(input, tool_name, fields, unknown_keys, &[], &mut issues)?;
    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(issue_error(issues))
    }
}

fn parse_object_fields(
    input: &Utf16JsonProjection,
    tool_name: &str,
    fields: &[OutputField],
    unknown_keys: UnknownOutputKeys,
    path: &[Utf16JsonProjection],
    issues: &mut Vec<Utf16JsonProjection>,
) -> Result<Utf16JsonProjection, ToolError> {
    let root = input.at_pointer("").expect("valid source root");
    if root.object_entries().is_none() {
        issues.push(type_issue("object", Some(root), path.to_vec()));
        return Ok(input.clone());
    }
    let mut retained = Vec::with_capacity(fields.len());
    let mut passthrough = input.clone();
    for field in fields {
        let value = root.get_property(field.name);
        if value.is_none() && !field.required {
            continue;
        }
        let mut field_path = path.to_vec();
        field_path.push(field.name.into());
        if !value.is_some_and(|value| matches_type(value, field.kind)) {
            let expected = match field.kind {
                OutputFieldType::String | OutputFieldType::StringOrNull => "string",
                OutputFieldType::Number => "number",
                OutputFieldType::Boolean => "boolean",
                OutputFieldType::Array => "array",
                OutputFieldType::Object | OutputFieldType::ObjectShape(_) => "object",
                OutputFieldType::False => {
                    issues.push(false_literal_issue(field_path));
                    continue;
                }
            };
            issues.push(type_issue(expected, value, field_path));
            continue;
        }
        let key = field.name.encode_utf16().collect::<Vec<_>>();
        let value = input
            .subprojection_property_units(&key)
            .expect("validated output property");
        let value = match field.kind {
            OutputFieldType::ObjectShape(nested) => parse_object_fields(
                &value,
                tool_name,
                nested,
                UnknownOutputKeys::Strip,
                &field_path,
                issues,
            )?,
            _ => value,
        };
        if matches!(unknown_keys, UnknownOutputKeys::Passthrough) {
            passthrough
                .set_field(field.name, value.clone())
                .map_err(|_| invalid(tool_name, "could not construct parsed output"))?;
        }
        retained.push((key, value));
    }
    if matches!(unknown_keys, UnknownOutputKeys::Passthrough) {
        return Ok(passthrough);
    }
    // Zod constructs known properties in schema order, independently of the
    // incoming object's insertion order. Nested shapes contribute issues at
    // their schema position rather than stopping after a top-level failure.
    Utf16JsonProjection::object_units(retained)
        .map_err(|_| invalid(tool_name, "could not construct parsed output"))
}

fn false_literal_issue(path: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(vec![
        ("code".into(), "invalid_value".into()),
        (
            "values".into(),
            Utf16JsonProjection::parse("[false]").expect("literal values"),
        ),
        (
            "path".into(),
            Utf16JsonProjection::array(path).expect("literal issue path"),
        ),
        ("message".into(), "Invalid input: expected false".into()),
    ])
    .expect("literal output issue")
}

pub fn string_array(
    input: &Utf16JsonProjection,
    pointer: &str,
    tool_name: &str,
    field: &str,
) -> Result<Vec<Utf16JsonProjection>, ToolError> {
    let value = input.at_pointer(pointer);
    let path = pointer_path(pointer);
    let Some(array) = value.and_then(JsonValueView::array_len) else {
        return Err(issue_error(vec![type_issue("array", value, path)]));
    };
    let mut values = Vec::with_capacity(array);
    let mut issues = Vec::new();
    for index in 0..array {
        let item_pointer = format!("{pointer}/{index}");
        let item = input
            .at_pointer(&item_pointer)
            .expect("validated output array");
        if item.as_str().is_none() {
            let mut item_path = path.clone();
            item_path.push(Utf16JsonProjection::parse(&index.to_string()).expect("array index"));
            issues.push(type_issue("string", Some(item), item_path));
        } else {
            values.push(input.subprojection(&item_pointer).map_err(|_| {
                invalid(tool_name, &format!("output field `{field}` is malformed"))
            })?);
        }
    }
    if issues.is_empty() {
        Ok(values)
    } else {
        Err(issue_error(issues))
    }
}

pub fn array_values(
    input: &Utf16JsonProjection,
    pointer: &str,
    tool_name: &str,
    field: &str,
) -> Result<Vec<Utf16JsonProjection>, ToolError> {
    let array = input
        .at_pointer(pointer)
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| {
            invalid(
                tool_name,
                &format!("output field `{field}` must be an array"),
            )
        })?;
    (0..array)
        .map(|index| {
            let item_pointer = format!("{pointer}/{index}");
            input
                .subprojection(&item_pointer)
                .map_err(|_| invalid(tool_name, &format!("output field `{field}` is malformed")))
        })
        .collect()
}

pub fn invalid(tool_name: &str, message: &str) -> ToolError {
    ToolError::InvalidInput(format!("{tool_name}: {message}").into())
}

fn pointer_path(pointer: &str) -> Vec<Utf16JsonProjection> {
    pointer
        .split('/')
        .skip(1)
        .map(|part| part.replace("~1", "/").replace("~0", "~").into())
        .collect()
}

fn issue_error(issues: Vec<Utf16JsonProjection>) -> ToolError {
    ToolError::InvalidInput(crate::native_schema::format_raw_issues(&issues))
}

fn type_issue(
    expected: &str,
    value: Option<JsonValueView<'_>>,
    path: Vec<Utf16JsonProjection>,
) -> Utf16JsonProjection {
    let received = match value {
        None => "undefined",
        Some(value) if value.is_null() => "null",
        Some(value) if value.as_str().is_some() => "string",
        Some(value) if value.as_bool().is_some() => "boolean",
        Some(value) if value.as_number().is_some() => {
            let number = value.as_number().expect("number");
            if number.is_nan() {
                "NaN"
            } else if number == f64::NEG_INFINITY {
                "-Infinity"
            } else if number == f64::INFINITY {
                "Infinity"
            } else {
                "number"
            }
        }
        Some(value) if value.array_len().is_some() => "array",
        Some(_) => "object",
    };
    let mut fields = vec![
        ("expected".into(), expected.into()),
        ("code".into(), "invalid_type".into()),
    ];
    if matches!(received, "NaN" | "Infinity" | "-Infinity") {
        fields.push(("received".into(), received.into()));
    }
    fields.push((
        "path".into(),
        Utf16JsonProjection::array(path).expect("output issue path"),
    ));
    fields.push((
        "message".into(),
        format!("Invalid input: expected {expected}, received {received}").into(),
    ));
    Utf16JsonProjection::object(fields).expect("output issue")
}

fn matches_type(value: JsonValueView<'_>, kind: OutputFieldType) -> bool {
    match kind {
        OutputFieldType::String => value.as_str().is_some(),
        OutputFieldType::Number => value.as_number().is_some_and(f64::is_finite),
        OutputFieldType::Boolean => value.as_bool().is_some(),
        OutputFieldType::Array => value.array_len().is_some(),
        OutputFieldType::Object | OutputFieldType::ObjectShape(_) => {
            value.object_entries().is_some()
        }
        OutputFieldType::StringOrNull => value.as_str().is_some() || value.is_null(),
        OutputFieldType::False => value.as_bool() == Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonBuilder;

    fn fixture(case: &str, file: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/parity/current-alignment/fixtures/tool-output-parser/native-baseline/2.1.293")
                .join(case)
                .join(file),
        ).unwrap()
    }

    fn returned_result(case: &str) -> Utf16JsonProjection {
        let marker = fixture(case, "hook-marker.raw.txt");
        let (_, json) = marker.split_once("NATIVE_GLOB_TOOL_CALL_HOOK=").unwrap();
        Utf16JsonProjection::parse(json.trim())
            .unwrap()
            .subprojection("/result")
            .unwrap()
    }

    fn glob_fields() -> [OutputField; 6] {
        [
            OutputField {
                name: "durationMs",
                kind: OutputFieldType::Number,
                required: true,
            },
            OutputField {
                name: "numFiles",
                kind: OutputFieldType::Number,
                required: true,
            },
            OutputField {
                name: "filenames",
                kind: OutputFieldType::Array,
                required: true,
            },
            OutputField {
                name: "truncated",
                kind: OutputFieldType::Boolean,
                required: true,
            },
            OutputField {
                name: "totalMatches",
                kind: OutputFieldType::Number,
                required: false,
            },
            OutputField {
                name: "countIsComplete",
                kind: OutputFieldType::Boolean,
                required: false,
            },
        ]
    }

    fn assert_native_issue_body(case: &str, error: ToolError) {
        let native = fixture(case, "model-tool-result-content.raw.txt");
        // The parser returns the issue body only. The surrounding Mod error
        // envelope and dynamic token reminder belong to the dispatcher gate.
        let body = native.strip_prefix(
            "<tool_use_error>tool.call step resolved Glob with a result that does not match its output shape: "
        ).unwrap().split_once("</tool_use_error>").unwrap().0;
        let actual =
            String::from_utf16(&error.model_facing_message().string_units("").unwrap()).unwrap();
        assert_eq!(actual.as_bytes(), body.as_bytes(), "{case}");
    }

    #[test]
    fn native_293_non_finite_and_missing_number_issue_bytes() {
        for (case, number) in [
            ("glob-duration-1e400", Some(f64::INFINITY)),
            ("glob-duration-infinity", Some(f64::INFINITY)),
            ("glob-duration-negative-infinity", Some(f64::NEG_INFINITY)),
            ("glob-duration-nan", Some(f64::NAN)),
            ("glob-duration-wrong-type", None),
            ("glob-duration-missing", None),
        ] {
            let mut input = returned_result(case);
            if let Some(number) = number {
                // The Native hook marker independently records String(number).
                // JSON logging writes null; restore its recorded JS number for
                // this parser gate, without claiming the worker IPC is tested.
                let mut builder = Utf16JsonBuilder::new();
                let root = builder.push_number(number);
                input
                    .set_field("durationMs", builder.finish(root).unwrap())
                    .unwrap();
            }
            let error =
                parse_object(&input, "Glob", &glob_fields(), UnknownOutputKeys::Strip).unwrap_err();
            assert_native_issue_body(case, error);
        }
    }

    #[test]
    fn native_293_array_item_issue_bytes() {
        let case = "glob-filenames-item-wrong-type";
        let error =
            string_array(&returned_result(case), "/filenames", "Glob", "filenames").unwrap_err();
        assert_native_issue_body(case, error);
    }

    #[test]
    fn native_293_schema_order_and_unknown_key_stripping_bytes() {
        for case in ["glob-known-reversed", "glob-unknown-top"] {
            let parsed = parse_object(
                &returned_result(case),
                "Glob",
                &glob_fields(),
                UnknownOutputKeys::Strip,
            )
            .unwrap();
            assert_eq!(
                parsed.to_json_string().unwrap().as_bytes(),
                fixture(case, "tool-use-result.raw.json").as_bytes(),
                "{case}"
            );
        }
    }
}
