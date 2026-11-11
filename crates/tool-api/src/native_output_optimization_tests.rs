//! Differential coverage and a manual scaling probe for validation-only arrays.

use super::*;

// Pre-optimization parsers, retained only as differential oracles.
fn legacy_parse_object(
    input: &Utf16JsonProjection,
    tool_name: &str,
    fields: &[OutputField],
    unknown_keys: UnknownOutputKeys,
) -> Result<Utf16JsonProjection, ToolError> {
    let mut issues = Vec::new();
    let parsed =
        legacy_parse_object_fields(input, tool_name, fields, unknown_keys, &[], &mut issues)?;
    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(issue_error(issues))
    }
}

fn legacy_parse_object_fields(
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
                OutputFieldType::StringEnum(_)
                | OutputFieldType::ObjectArray(_)
                | OutputFieldType::StringArray => {
                    unreachable!("recursive array and enum rules postdate the legacy oracle")
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
            OutputFieldType::ObjectShape(nested) => legacy_parse_object_fields(
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

fn legacy_string_array(
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

#[test]
fn validation_only_string_arrays_match_legacy_diagnostics() {
    for raw in [
        r#"{}"#,
        r#"{"filenames":null}"#,
        r#"{"filenames":{"0":"file"}}"#,
        r#"{"filenames":[]}"#,
        r#"{"filenames":["file\ud800","file\udc00"]}"#,
        r#"{"filenames":["ok",null,3,{},[],true,"last",false]}"#,
        r#"{"filenames":[1e400,-1e400]}"#,
    ] {
        let data = Utf16JsonProjection::parse(raw).unwrap();
        let before = legacy_string_array(&data, "/filenames", "Glob", "filenames").map(|_| ());
        let after = validate_string_array(&data, "/filenames", "Glob", "filenames");
        match (before, after) {
            (Ok(()), Ok(())) => {}
            (Err(before), Err(after)) => assert_eq!(
                after.model_facing_message().to_json_string().unwrap(),
                before.model_facing_message().to_json_string().unwrap(),
                "{raw}"
            ),
            (before, after) => panic!(
                "different validation result for {raw}: legacy={}, optimized={}",
                before.is_ok(),
                after.is_ok()
            ),
        }
    }
}

#[test]
#[ignore = "manual scaling probe; prints timings without timing assertions"]
fn string_array_validation_scaling_probe() {
    for count in [128usize, 512, 2048] {
        let filenames: Vec<_> = (0..count)
            .map(|index| format!("src/file-{index}.rs"))
            .collect();
        let data =
            Utf16JsonProjection::parse(&serde_json::json!({"filenames":filenames}).to_string())
                .unwrap();
        let started = std::time::Instant::now();
        let before = legacy_string_array(&data, "/filenames", "Glob", "filenames").unwrap();
        let legacy_us = started.elapsed().as_micros();
        let started = std::time::Instant::now();
        validate_string_array(&data, "/filenames", "Glob", "filenames").unwrap();
        let optimized_us = started.elapsed().as_micros();
        assert_eq!(before.len(), count);
        // The baseline calls compact_nodes once per item. Each call initializes
        // visited/remap arrays for this fixture's count + 2 source arena nodes.
        println!("{{\"probe\":\"string_array_validation\",\"items\":{count},\"legacy_item_projections\":{count},\"optimized_item_projections\":0,\"legacy_arena_index_slots_initialized\":{},\"optimized_arena_index_slots_initialized\":0,\"legacy_us\":{legacy_us},\"optimized_us\":{optimized_us}}}", 2 * count * (count + 2));
    }
}

#[test]
fn borrowed_output_field_and_array_materialization_matches_legacy() {
    static NESTED: &[OutputField] = &[OutputField {
        name: "value",
        kind: OutputFieldType::String,
        required: true,
    }];
    let fields = [
        OutputField {
            name: "items",
            kind: OutputFieldType::Array,
            required: true,
        },
        OutputField {
            name: "child",
            kind: OutputFieldType::ObjectShape(NESTED),
            required: true,
        },
        OutputField {
            name: "number",
            kind: OutputFieldType::Number,
            required: false,
        },
    ];
    for raw in [
        r#"{"extra":{"\ud800":"retained"},"child":{"ignored":1,"value":"\ud800"},"items":["\udc00",{"\ud800":-0.0,"2":1e-7,"1":2},[1,null]],"number":0}"#,
        r#"{"items":["a"],"child":{"value":true},"number":1e400}"#,
        r#"{"items":null,"child":{"value":null}}"#,
        r#"[]"#,
    ] {
        let data = Utf16JsonProjection::parse(raw).unwrap();
        for unknown in [UnknownOutputKeys::Strip, UnknownOutputKeys::Passthrough] {
            let before = legacy_parse_object(&data, "Test", &fields, unknown);
            let after = parse_object(&data, "Test", &fields, unknown);
            match (before, after) {
                (Ok(before), Ok(after)) => assert_eq!(
                    after.to_json_string().unwrap(),
                    before.to_json_string().unwrap(),
                    "{raw}"
                ),
                (Err(before), Err(after)) => assert_eq!(
                    after.model_facing_message().to_json_string().unwrap(),
                    before.model_facing_message().to_json_string().unwrap(),
                    "{raw}"
                ),
                (before, after) => panic!(
                    "different parse result: legacy={}, optimized={}",
                    before.is_ok(),
                    after.is_ok()
                ),
            }
        }
        if let Some(count) = data.at_pointer("/items").and_then(JsonValueView::array_len) {
            let before: Vec<_> = (0..count)
                .map(|index| {
                    data.subprojection(&format!("/items/{index}"))
                        .unwrap()
                        .to_json_string()
                        .unwrap()
                })
                .collect();
            let after: Vec<_> = array_values(&data, "/items", "Test", "items")
                .unwrap()
                .iter()
                .map(|item| item.to_json_string().unwrap())
                .collect();
            assert_eq!(after, before);
        }
    }
    let data = Utf16JsonProjection::parse(r#"{"items":["one\ud800","two\udc00"]}"#).unwrap();
    let before = legacy_string_array(&data, "/items", "Test", "items").unwrap();
    let after = string_array(&data, "/items", "Test", "items").unwrap();
    assert_eq!(
        after
            .iter()
            .map(|item| item.string_units("").unwrap())
            .collect::<Vec<_>>(),
        before
            .iter()
            .map(|item| item.string_units("").unwrap())
            .collect::<Vec<_>>()
    );
}
