//! JSON-Schema input validation for tool dispatch, adapted to source-backed
//! tool schemas and inputs. Rich schemas use an ephemeral reversible consumer
//! namespace; diagnostics are restored to UTF-16 source projections before
//! crossing this module boundary.

use boon::{Compiler, Schemas, ValidationError};
use lingxi_core::types::utf16_json::{Utf16JsonConsumer, Utf16JsonProjection};
use tool_api::native_schema::{
    format_issues as format_source_issues, format_raw_issues, NativeSchemaError,
};
use tool_api::Tool;

fn js_keys(object: &serde_json::Map<String, serde_json::Value>) -> Vec<&String> {
    fn index(key: &str) -> Option<u32> {
        let n = key.parse::<u32>().ok()?;
        (n != u32::MAX && n.to_string() == key).then_some(n)
    }
    let mut keys: Vec<_> = object.keys().collect();
    keys.sort_by_key(|key| index(key).map_or((1, 0), |n| (0, n)));
    keys
}

fn js_number(value: &serde_json::Number) -> String {
    let n = value
        .as_f64()
        .expect("JSON number has an f64 representation");
    if n == 0.0 {
        return "0".into();
    }
    if !n.is_finite() {
        return "null".into();
    }
    if n.abs() >= 1e21 || n.abs() < 1e-6 {
        let scientific = format!("{n:e}");
        let (mantissa, exponent) = scientific.split_once('e').expect("scientific exponent");
        let exponent: i32 = exponent.parse().expect("numeric exponent");
        format!(
            "{mantissa}e{}{exponent}",
            if exponent >= 0 { "+" } else { "" }
        )
    } else {
        n.to_string()
    }
}

/// JSON.stringify(value, null, 2), including JS Number and property ordering.
fn js_json(value: &serde_json::Value, pretty: bool) -> String {
    fn render(value: &serde_json::Value, level: usize, pretty: bool) -> String {
        use serde_json::Value;
        match value {
            Value::Number(n) => js_number(n),
            Value::Array(array) if !array.is_empty() => {
                let values: Vec<_> = array.iter().map(|v| render(v, level + 1, pretty)).collect();
                if pretty {
                    format!(
                        "[\n{}{}\n{}]",
                        "  ".repeat(level + 1),
                        values.join(&format!(",\n{}", "  ".repeat(level + 1))),
                        "  ".repeat(level)
                    )
                } else {
                    format!("[{}]", values.join(","))
                }
            }
            Value::Object(object) if !object.is_empty() => {
                let values: Vec<_> = js_keys(object)
                    .into_iter()
                    .map(|key| {
                        format!(
                            "{}:{}{}",
                            serde_json::to_string(key).unwrap(),
                            if pretty { " " } else { "" },
                            render(&object[key], level + 1, pretty)
                        )
                    })
                    .collect();
                if pretty {
                    format!(
                        "{{\n{}{}\n{}}}",
                        "  ".repeat(level + 1),
                        values.join(&format!(",\n{}", "  ".repeat(level + 1))),
                        "  ".repeat(level)
                    )
                } else {
                    format!("{{{}}}", values.join(","))
                }
            }
            _ => serde_json::to_string(value).expect("JSON value"),
        }
    }
    render(value, 0, pretty)
}

fn js_value_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    if left.is_number() && right.is_number() {
        left.as_f64() == right.as_f64()
    } else {
        left == right
    }
}

#[cfg(test)]
fn issue_path(path: &[serde_json::Value]) -> String {
    let mut result = String::new();
    for (index, part) in path.iter().enumerate() {
        if let Some(key) = part.as_str() {
            if index != 0 {
                result.push('.');
            }
            result.push_str(key);
        } else {
            result.push_str(&format!("[{part}]"));
        }
    }
    result
}

#[cfg(test)]
fn format_issues(name: &str, issues: &[serde_json::Value]) -> String {
    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    let mut types = Vec::new();
    for issue in issues {
        let path = issue_path(issue["path"].as_array().map_or(&[][..], Vec::as_slice));
        let message = issue["message"].as_str().unwrap_or("");
        match issue["code"].as_str() {
            Some("invalid_type") if message.contains("received undefined") => {
                missing.push(format!("The required parameter `{path}` is missing"))
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
                let expected = issue["expected"].as_str().unwrap_or("undefined");
                types.push(format!("The parameter `{path}` type is expected as `{expected}` but provided as `{received}`"));
            }
            Some("unrecognized_keys") => {
                if let Some(keys) = issue["keys"].as_array() {
                    unexpected.extend(
                        keys.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(|key| format!("An unexpected parameter `{key}` was provided")),
                    );
                }
            }
            _ => {}
        }
    }
    let messages: Vec<_> = missing.into_iter().chain(unexpected).chain(types).collect();
    if messages.is_empty() {
        js_json(&serde_json::Value::Array(issues.to_vec()), true)
    } else {
        format!(
            "{name} failed due to the following {}:\n{}",
            if messages.len() == 1 {
                "issue"
            } else {
                "issues"
            },
            messages.join("\n")
        )
    }
}

fn value_type(value: Option<&serde_json::Value>) -> &'static str {
    use serde_json::Value;
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn quoted_options(values: &[serde_json::Value], separator: &str) -> String {
    values
        .iter()
        .map(|value| js_json(value, false))
        .collect::<Vec<_>>()
        .join(separator)
}

/// Recursively flatten a boon [`ValidationError`] into concise leaf messages.
fn flatten(err: &ValidationError, out: &mut Vec<String>) {
    if err.causes.is_empty() {
        let loc = err.instance_location.to_string();
        let loc = if loc.is_empty() {
            "(root)".to_string()
        } else {
            loc
        };
        out.push(format!("at '{loc}': {}", err.kind));
    } else {
        for cause in &err.causes {
            flatten(cause, out);
        }
    }
}

/// Validate a tool input against JSON Schema. Malformed tool schemas keep the
/// established warn-and-pass behavior because they are host schema bugs.
pub(crate) fn validate_tool_input_schema(
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<(), String> {
    const URL: &str = "mem://tool-input-schema";
    let mut schemas = Schemas::new();
    let mut compiler = Compiler::new();
    if let Err(error) = compiler.add_resource(URL, schema.clone()) {
        tracing::warn!(error = %error, "tool input_schema failed to load (treating as PASS — host schema bug)");
        return Ok(());
    }
    let schema_index = match compiler.compile(URL, &mut schemas) {
        Ok(index) => index,
        Err(error) => {
            tracing::warn!(error = %error, "tool input_schema failed to compile (treating as PASS — host schema bug)");
            return Ok(());
        }
    };
    if let Err(error) = schemas.validate(input, schema_index) {
        let mut messages = Vec::new();
        flatten(&error, &mut messages);
        if messages.is_empty() {
            messages.push(error.kind.to_string());
        }
        return Err(messages.join("; "));
    }
    Ok(())
}

fn source_text(text: String) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(text.clone(), text.encode_utf16().collect())
        .expect("Rust strings are valid UTF-16 projections")
}

fn source_error(message: String) -> NativeSchemaError {
    let display = source_text(message);
    NativeSchemaError {
        raw: display.clone(),
        display,
    }
}

/// Native input parsing/normalization remains authoritative where a tool has
/// opted into it. Otherwise the rich Zod-compatible collector validates the
/// reversible temporary values, then restores every issue to exact source.
pub(crate) fn validate_tool_schema_detailed(
    tool: &dyn Tool,
    input: &Utf16JsonProjection,
) -> Result<(), NativeSchemaError> {
    validate_tool_schema_projected(tool, input)
}

/// Validate a source-backed input without materializing a persistent Value
/// shadow. `Utf16JsonConsumer` is only the validator's temporary namespace.
pub(crate) fn validate_tool_schema_projected(
    tool: &dyn Tool,
    input: &Utf16JsonProjection,
) -> Result<(), NativeSchemaError> {
    let schema = tool.input_validation_schema();
    let parsed = tool.parse_native_input(input);
    if let Some(Err(error)) = &parsed {
        return Err(NativeSchemaError {
            raw: error.raw.clone(),
            display: error.display.clone(),
        });
    }

    let native_issues = tool.input_validation_issues(input);
    if (has_unpaired_source_units(schema) || has_unpaired_source_units(input))
        && schema_uses_regex(schema)
    {
        return Err(source_error(
            "Exact UTF-16 schema regex validation is unavailable".into(),
        ));
    }

    let mut sources = Vec::with_capacity(2 + native_issues.len());
    sources.push(schema);
    sources.push(input);
    sources.extend(native_issues.iter());
    let consumer = Utf16JsonConsumer::new(&sources)
        .map_err(|error| source_error(format!("Could not validate tool input: {error}")))?;
    let values = consumer.values();
    let schema_value = &values[0];
    let input_value = &values[1];
    let native_values = &values[2..];

    if parsed.is_some() {
        return format_native_issues(tool.name(), native_values, &consumer);
    }

    validate_tool_schema_values(
        tool.name(),
        schema_value,
        input_value,
        native_values,
        &consumer,
    )
}

fn has_unpaired_source_units(value: &Utf16JsonProjection) -> bool {
    let Some(root) = value.at_pointer("") else {
        return false;
    };
    let mut pending = vec![root];
    while let Some(current) = pending.pop() {
        if current
            .code_units()
            .is_some_and(|units| String::from_utf16(units).is_err())
        {
            return true;
        }
        if let Some(entries) = current.object_entries() {
            for entry in entries {
                if String::from_utf16(entry.code_units).is_err() {
                    return true;
                }
                pending.push(entry.value);
            }
        } else if let Some(length) = current.array_len() {
            pending.extend((0..length).filter_map(|index| current.get_index(index)));
        }
    }
    false
}

fn schema_uses_regex(schema: &Utf16JsonProjection) -> bool {
    let Some(root) = schema.at_pointer("") else {
        return false;
    };
    let pattern = "pattern".encode_utf16().collect::<Vec<_>>();
    let pattern_properties = "patternProperties".encode_utf16().collect::<Vec<_>>();
    let mut pending = vec![root];
    while let Some(value) = pending.pop() {
        if let Some(entries) = value.object_entries() {
            for entry in entries {
                if entry.code_units == pattern || entry.code_units == pattern_properties {
                    return true;
                }
                pending.push(entry.value);
            }
        } else if let Some(length) = value.array_len() {
            pending.extend((0..length).filter_map(|index| value.get_index(index)));
        }
    }
    false
}

fn restore_issues(
    issues: &[serde_json::Value],
    consumer: &Utf16JsonConsumer,
) -> Result<Vec<Utf16JsonProjection>, NativeSchemaError> {
    issues
        .iter()
        .map(|issue| {
            consumer.restore(issue).map_err(|error| {
                source_error(format!("Could not restore validation issue: {error}"))
            })
        })
        .collect()
}

fn format_native_issues(
    tool_name: &str,
    issues: &[serde_json::Value],
    consumer: &Utf16JsonConsumer,
) -> Result<(), NativeSchemaError> {
    if issues.is_empty() {
        return Ok(());
    }
    let restored = restore_issues(issues, consumer)?;
    Err(NativeSchemaError {
        display: format_source_issues(tool_name, &restored),
        raw: format_raw_issues(&restored),
    })
}

fn validate_tool_schema_values(
    tool_name: &str,
    schema: &serde_json::Value,
    input: &serde_json::Value,
    native_issues: &[serde_json::Value],
    consumer: &Utf16JsonConsumer,
) -> Result<(), NativeSchemaError> {
    let mut issues = Vec::new();
    let fallback = validate_tool_input_schema(schema, input).err();
    if !collect_issues(schema, Some(input), &[], &mut issues) {
        return match fallback {
            Some(display) => Err(restore_text_error(display, consumer)),
            None if native_issues.is_empty() => Ok(()),
            None => format_native_issues(tool_name, native_issues, consumer),
        };
    }

    let structural_count = issues.len();
    for native in native_issues {
        if let Some(existing) = issues[..structural_count]
            .iter_mut()
            .find(|issue| issue["code"] == native["code"] && issue["path"] == native["path"])
        {
            *existing = native.clone();
        } else {
            issues.push(native.clone());
        }
    }
    if issues.is_empty() {
        return Ok(());
    }
    issues.sort_by_key(|issue| {
        declaration_order(
            schema,
            issue["path"].as_array().map_or(&[][..], Vec::as_slice),
        )
    });
    format_native_issues(tool_name, &issues, consumer)
}

fn restore_text_error(message: String, consumer: &Utf16JsonConsumer) -> NativeSchemaError {
    let temporary = serde_json::Value::String(message.clone());
    let restored = consumer
        .restore(&temporary)
        .unwrap_or_else(|_| source_text(message));
    NativeSchemaError {
        raw: restored.clone(),
        display: restored,
    }
}

/// Display projection used by model-facing rejection paths.
pub(crate) fn validate_tool_schema(
    tool: &dyn Tool,
    input: &Utf16JsonProjection,
) -> Result<(), Utf16JsonProjection> {
    validate_tool_schema_detailed(tool, input).map_err(|error| error.display)
}

fn declaration_order(schema: &serde_json::Value, path: &[serde_json::Value]) -> Vec<usize> {
    let mut schema = schema;
    let mut order = Vec::new();
    for part in path {
        if let Some(key) = part.as_str() {
            let properties = schema
                .get("properties")
                .and_then(serde_json::Value::as_object);
            order.push(
                properties
                    .and_then(|props| js_keys(props).into_iter().position(|name| name == key))
                    .unwrap_or(usize::MAX),
            );
            schema = properties
                .and_then(|props| props.get(key))
                .unwrap_or(&serde_json::Value::Null);
        } else {
            order.push(
                part.as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(usize::MAX),
            );
            schema = schema.get("items").unwrap_or(&serde_json::Value::Null);
        }
    }
    // Object refinements run after child schemas (a parent path is last).
    order.push(usize::MAX);
    order
}

fn collect_issues(
    schema: &serde_json::Value,
    input: Option<&serde_json::Value>,
    path: &[serde_json::Value],
    issues: &mut Vec<serde_json::Value>,
) -> bool {
    use serde_json::{json, Value};
    let Some(object) = schema.as_object() else {
        return schema == &json!(true);
    };
    // Unexported Zod metadata comes from Tool::input_validation_issues. Never
    // manufacture errors for unknown JSON Schema vocabulary.
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "title"
                | "description"
                | "$schema"
                | "default"
                | "enum"
                | "const"
                | "anyOf"
                | "oneOf"
                | "pattern"
                | "format"
                | "minLength"
                | "maxLength"
                | "minItems"
                | "maxItems"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
        )
    }) {
        return false;
    }
    if let Some(branches) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    {
        let mut errors = Vec::new();
        let mut dirty = None;
        for branch in branches {
            let mut branch_issues = Vec::new();
            if !collect_issues(branch, input, &[], &mut branch_issues) {
                return false;
            }
            if branch_issues.is_empty() {
                return true;
            }
            if dirty.is_none()
                && branch_issues.iter().all(|issue| {
                    !matches!(
                        issue["code"].as_str(),
                        Some("invalid_type" | "invalid_value" | "invalid_union")
                    )
                })
            {
                dirty = Some(branch_issues.clone());
            }
            errors.push(json!(branch_issues));
        }
        if let Some(mut dirty) = dirty {
            for issue in &mut dirty {
                let mut full = path.to_vec();
                full.extend(issue["path"].as_array().into_iter().flatten().cloned());
                issue["path"] = json!(full);
            }
            issues.extend(dirty);
        } else {
            issues.push(json!({"code":"invalid_union","errors":errors,"path":path,"message":"Invalid input"}));
        }
        return true;
    }
    let values = schema
        .get("enum")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| schema.get("const").map(|value| vec![value.clone()]));
    if let Some(values) = values {
        if !values
            .iter()
            .any(|value| input.is_some_and(|input| js_value_eq(input, value)))
        {
            let message = if values.len() == 1 {
                format!("Invalid input: expected {}", js_json(&values[0], false))
            } else {
                format!(
                    "Invalid option: expected one of {}",
                    quoted_options(&values, "|")
                )
            };
            issues.push(
                json!({"code":"invalid_value","values":values,"path":path,"message":message}),
            );
        }
        return true;
    }
    let received = value_type(input);
    let expected = schema.get("type").and_then(Value::as_str);
    if schema.get("type").is_some() && expected.is_none() {
        return false;
    }
    if let Some(expected) = expected {
        if !matches!(
            expected,
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        ) {
            return false;
        }
        let matches = expected == received
            || (expected == "integer"
                && input.is_some_and(|v| v.as_f64().is_some_and(|n| n.fract() == 0.0)));
        if !matches {
            if expected == "integer" && received == "number" {
                issues.push(json!({"expected":"int","format":"safeint","code":"invalid_type","path":path,"message":"Invalid input: expected int, received number"}));
            } else {
                issues.push(json!({"expected":expected,"code":"invalid_type","path":path,"message":format!("Invalid input: expected {expected}, received {received}")}));
            }
            return true;
        }
    }

    let Some(input) = input else {
        return false;
    };
    if let Some(input) = input.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        let required = schema.get("required").and_then(Value::as_array);
        if required.is_some_and(|keys| {
            keys.iter().any(|key| {
                !properties
                    .is_some_and(|props| key.as_str().is_some_and(|key| props.contains_key(key)))
            })
        }) {
            return false;
        }
        if let Some(properties) = properties {
            for key in js_keys(properties) {
                let child = &properties[key];
                let value = input.get(key);
                if value.is_none()
                    && !required.is_some_and(|keys| keys.iter().any(|v| v.as_str() == Some(key)))
                {
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(json!(key));
                if !collect_issues(child, value, &child_path, issues) {
                    return false;
                }
            }
        }
        if schema.get("additionalProperties") == Some(&json!(false)) {
            let keys: Vec<_> = js_keys(input)
                .into_iter()
                // Native Zod intentionally ignores __proto__ while walking
                // object keys, even for strict objects.
                .filter(|key| {
                    key.as_str() != "__proto__" && !properties.is_some_and(|p| p.contains_key(*key))
                })
                .cloned()
                .collect();
            if !keys.is_empty() {
                issues.push(json!({"code":"unrecognized_keys","keys":keys,"path":path,"message":format!("Unrecognized key{}: {}", if keys.len() == 1 { "" } else { "s" }, keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "))}));
            }
        } else if let Some(extra_schema) =
            schema.get("additionalProperties").filter(|v| v.is_object())
        {
            for (key, value) in input {
                if properties.is_some_and(|props| props.contains_key(key)) {
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(json!(key));
                if !collect_issues(extra_schema, Some(value), &child_path, issues) {
                    return false;
                }
            }
        }
    }
    if let (Some(value), Some(pattern)) = (
        input.as_str(),
        schema.get("pattern").and_then(Value::as_str),
    ) {
        let Ok(regex) = regex::Regex::new(pattern) else {
            return false;
        };
        if !regex.is_match(value) {
            issues.push(json!({"origin":"string","code":"invalid_format","format":"regex","pattern":format!("/{pattern}/"),"path":path,"message":format!("Invalid string: must match pattern /{pattern}/")}));
        }
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str) {
        if format != "uri" {
            return false;
        }
        if input
            .as_str()
            .is_some_and(|value| url::Url::parse(value).is_err())
        {
            issues.push(
                json!({"code":"invalid_format","format":"url","path":path,"message":"Invalid URL"}),
            );
        }
    }
    if let (Some(array), Some(items)) = (input.as_array(), schema.get("items")) {
        for (index, value) in array.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(json!(index));
            if !collect_issues(items, Some(value), &child_path, issues) {
                return false;
            }
        }
    }
    // Zod's string length checks count UTF-16 code units, as JavaScript does.
    let measure = input
        .as_str()
        .map(|s| ("string", s.encode_utf16().count() as f64))
        .or_else(|| input.as_array().map(|v| ("array", v.len() as f64)))
        .or_else(|| input.as_f64().map(|n| ("number", n)));
    if let Some((kind, value)) = measure {
        for (key, small, inclusive) in [
            ("minLength", true, true),
            ("maxLength", false, true),
            ("minItems", true, true),
            ("maxItems", false, true),
            ("minimum", true, true),
            ("maximum", false, true),
            ("exclusiveMinimum", true, false),
            ("exclusiveMaximum", false, false),
        ] {
            let Some(limit) = schema.get(key) else {
                continue;
            };
            let Some(bound) = limit.as_f64() else {
                return false;
            };
            let fails = if small {
                value < bound || (!inclusive && value == bound)
            } else {
                value > bound || (!inclusive && value == bound)
            };
            if !fails {
                continue;
            }
            let display_limit = limit
                .as_number()
                .map_or_else(|| limit.to_string(), js_number);
            let relation = if small {
                if inclusive {
                    ">="
                } else {
                    ">"
                }
            } else if inclusive {
                "<="
            } else {
                "<"
            };
            let size = if small { "small" } else { "big" };
            let message = match kind {
                "string" => {
                    format!(
                        "Too {size}: expected string to have {relation}{display_limit} characters"
                    )
                }
                "array" => {
                    format!("Too {size}: expected array to have {relation}{display_limit} items")
                }
                _ => format!("Too {size}: expected number to be {relation}{display_limit}"),
            };
            let mut issue = serde_json::Map::new();
            issue.insert("origin".into(), json!(kind));
            issue.insert(
                "code".into(),
                json!(if small { "too_small" } else { "too_big" }),
            );
            issue.insert(
                if small { "minimum" } else { "maximum" }.into(),
                limit.clone(),
            );
            issue.insert("inclusive".into(), json!(inclusive));
            issue.insert("path".into(), json!(path));
            issue.insert("message".into(), json!(message));
            issues.push(Value::Object(issue));
        }
    }

    true
}

/// Validate a `PostToolUse`-hook `updatedToolOutput` replacement against the
/// tool's declared output JSON Schema (claude-code's `outputSchema.safeParse`,
/// BIN off 202169384: `e.outputSchema?.safeParse(D.updatedToolOutput)
/// ?.success!==!1`). Returns the concise error detail on a schema MISMATCH so
/// the caller can surface the `... does not match <tool>'s output shape ...`
/// meta message (BIN off 202465455) and keep the original output.
///
/// Identical compile/validate logic to [`validate_tool_input_schema`]: a
/// schema that fails to COMPILE is treated as PASS (a `LingXi` schema bug must
/// not discard a hook's valid replacement), matching the `?.` short-circuit in
/// the binary (`success!==!1` is also satisfied when `safeParse` is `undefined`
/// because `outputSchema` is absent — handled by the caller's `None` branch).
#[cfg(test)]
pub(crate) fn validate_tool_output_schema(
    schema: &serde_json::Value,
    output: &serde_json::Value,
) -> Result<(), String> {
    tool_api::output_schema::validate(schema, output)
}

#[cfg(test)]
mod tests {
    use super::{
        validate_tool_input_schema, validate_tool_output_schema, validate_tool_schema_detailed,
    };
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde_json::{json, Value};
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, ToolCallResult, ToolError, ToolInputRepairMetadata,
        ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
    };

    struct NativeCronSchema {
        name: String,
        schema: Utf16JsonProjection,
        refinement_issue: Option<Utf16JsonProjection>,
        native_input_validation: bool,
    }

    #[async_trait::async_trait]
    impl tool_api::Tool for NativeCronSchema {
        fn name(&self) -> &str {
            &self.name
        }

        fn input_schema(&self) -> &Utf16JsonProjection {
            &self.schema
        }

        fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
            ToolInputRepairMetadata::Known {
                schema: ToolInputRepairSchema::Advertised,
                schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }

        fn native_input_validation(&self) -> bool {
            self.native_input_validation
        }

        fn input_validation_issues(&self, _: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
            self.refinement_issue.iter().cloned().collect()
        }

        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }

        fn max_result_size_chars(&self) -> usize {
            1000
        }

        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }

        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }

        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            unreachable!("fixture tool is never executed")
        }

        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            unreachable!("fixture tool is never prompted")
        }

        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            unreachable!("fixture tool is never permission-checked")
        }

        async fn call(
            &self,
            _: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            unreachable!("fixture tool is never executed")
        }
    }

    fn source(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("UTF-8 fixture is valid JSON")
    }

    fn validate_rich_input_schema(name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        let tool = NativeCronSchema {
            name: name.into(),
            schema: source(schema),
            refinement_issue: None,
            native_input_validation: false,
        };
        validate_tool_schema_detailed(&tool, &source(input)).map_err(|error| {
            error
                .display
                .string_at("")
                .expect("schema validation display is a string")
                .to_owned()
        })
    }

    #[test]
    fn projected_schema_matches_exact_keys_required_enums_and_utf16_lengths() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Projection;
        let projection = Projection::parse(r#"{"type":"object","properties":{"\ud800":{"type":"string","enum":["\ud801"],"minLength":1,"maxLength":1},"\ue000":{"type":"string"}},"required":["\ud800"],"additionalProperties":false}"#).unwrap();
        let tool = NativeCronSchema {
            name: "Exact".into(),
            schema: projection,
            refinement_issue: None,
            native_input_validation: true,
        };
        for raw in [
            r#"{"\ud800":"\ud801"}"#,
            r#"{"\ue000":"plain","\ud800":"\ud801"}"#,
        ] {
            assert!(
                super::validate_tool_schema_projected(&tool, &Projection::parse(raw).unwrap())
                    .is_ok(),
                "{raw}"
            );
        }
        for raw in [
            r#"{"\ud801":"\ud801"}"#,
            r#"{"\ud800":"\ud800"}"#,
            r#"{"\ue000":"\ud801"}"#,
            r#"{"\ud800":"\ud801","\ud802":"extra"}"#,
        ] {
            assert!(
                super::validate_tool_schema_projected(&tool, &Projection::parse(raw).unwrap())
                    .is_err(),
                "{raw}"
            );
        }
        let projection = Projection::parse(
            r#"{"type":"object","properties":{"value":{"type":"string","pattern":"\\uE000"}}}"#,
        )
        .unwrap();
        let tool = NativeCronSchema {
            name: "Exact".into(),
            schema: projection,
            refinement_issue: None,
            native_input_validation: true,
        };
        let error = super::validate_tool_schema_projected(
            &tool,
            &Projection::parse(r#"{"value":"\ud800"}"#).unwrap(),
        )
        .unwrap_err();
        assert!(error
            .display
            .string_at("")
            .is_some_and(|message| message.contains("regex validation is unavailable")));
    }

    #[test]
    fn source_rich_schema_validation_keeps_nested_union_and_surrogate_diagnostics() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Projection;

        let schema = Projection::parse(
            r#"{"type":"object","properties":{"\ud800":{"type":"string","enum":["\ud801"]}},"required":["\ud800"],"additionalProperties":false}"#,
        )
        .unwrap();
        let tool = NativeCronSchema {
            name: "Exact".into(),
            schema,
            refinement_issue: None,
            native_input_validation: true,
        };
        let input = Projection::parse(r#"{"\ud800":"\ud802"}"#).unwrap();
        let error = super::validate_tool_schema_projected(&tool, &input).unwrap_err();
        let raw_text = error.raw.string_at("").expect("raw diagnostic string");
        let restored = Projection::parse(raw_text).expect("diagnostic JSON remains valid");
        assert_eq!(restored.string_units("/0/path/0"), Some(vec![0xd800]));
        assert_eq!(restored.string_units("/0/values/0"), Some(vec![0xd801]));

        let union_tool = NativeCronSchema {
            name: "Union".into(),
            schema: Projection::parse(
                r#"{"anyOf":[{"type":"string","minLength":5},{"type":"number"}]}"#,
            )
            .unwrap(),
            refinement_issue: None,
            native_input_validation: true,
        };
        assert!(super::validate_tool_schema_projected(
            &union_tool,
            &Projection::parse(r#""a""#).unwrap()
        )
        .is_err());

        let nested_schema = Projection::parse(
            r#"{"type":"object","properties":{"nested":{"type":"object","properties":{"name":{"type":"string","minLength":3}},"required":["name"]}}}"#,
        )
        .unwrap();
        let nested_tool = NativeCronSchema {
            name: "Nested".into(),
            schema: nested_schema,
            refinement_issue: None,
            native_input_validation: true,
        };
        assert!(super::validate_tool_schema_projected(
            &nested_tool,
            &Projection::parse(r#"{"nested":{"name":"ab"}}"#).unwrap()
        )
        .is_err());
    }

    #[test]
    fn source_native_gate_preserves_lone_surrogate_input_values() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Projection;

        let tool = NativeCronSchema {
            name: "Read".into(),
            schema: Projection::parse(
                r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
            )
            .unwrap(),
            refinement_issue: None,
            native_input_validation: true,
        };
        let input = Projection::parse(r#"{"path":"/tmp/\ud800.txt"}"#).unwrap();
        assert!(validate_tool_schema_detailed(&tool, &input).is_ok());
        assert_eq!(
            input.string_units("/path"),
            Some(vec![47, 116, 109, 112, 47, 0xd800, 46, 116, 120, 116])
        );
    }

    #[test]
    fn native_refinement_issue_survives_source_consumer_roundtrip() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Projection;

        let tool = NativeCronSchema {
            name: "Fixture".into(),
            schema: Projection::parse(r#"{"type":"object"}"#).unwrap(),
            refinement_issue: Some(
                Projection::parse(
                    r#"{"code":"custom","path":["field"],"message":"bad \ud800 value"}"#,
                )
                .unwrap(),
            ),
            native_input_validation: true,
        };
        let error =
            validate_tool_schema_detailed(&tool, &Projection::parse("{}").unwrap()).unwrap_err();
        let raw_text = error.raw.string_at("").expect("raw diagnostic string");
        let restored = Projection::parse(raw_text).expect("diagnostic JSON remains valid");
        assert_eq!(
            restored.string_units("/0/message"),
            Some(
                "bad "
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(" value".encode_utf16())
                    .collect()
            )
        );
    }

    #[test]
    fn cron_outer_diagnostics_match_executed_native_2_1_270_zod() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/cron/tests/fixtures/malformed_input_2_1_270.json"
        ))
        .unwrap();
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            let name = case["tool"].as_str().unwrap();
            let tool = NativeCronSchema {
                name: name.into(),
                schema: source(&oracle["schemas"][name]),
                refinement_issue: None,
                native_input_validation: true,
            };
            // The companion tool-cron test locks the real Tool::coerce_input
            // output to these actual native preprocessors before this gate.
            let result = validate_tool_schema_detailed(&tool, &source(&case["coerced"]));
            if case["success"] == true {
                assert!(result.is_ok(), "case {index}: {case}: {result:?}");
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    error.display.string_at(""),
                    case["display"].as_str(),
                    "display case {index}"
                );
                assert_eq!(
                    error.raw.string_at(""),
                    case["raw"].as_str(),
                    "raw case {index}"
                );
            }
        }
    }

    #[tokio::test]
    async fn cron_main_dispatch_keeps_native_raw_and_model_diagnostics_distinct() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/cron/tests/fixtures/malformed_input_2_1_270.json"
        ))
        .unwrap();
        let mut registry = tool_api::registry::ToolRegistry::new();
        for name in ["ScheduleWakeup", "CronCreate", "CronDelete", "CronList"] {
            registry.register_builtin(Arc::new(NativeCronSchema {
                name: name.into(),
                schema: source(&oracle["schemas"][name]),
                refinement_issue: None,
                native_input_validation: true,
            }));
        }
        let orch = crate::conversation::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::path::PathBuf::from("/tmp"),
        );
        for (index, case) in oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, case)| case["success"] == false)
        {
            let id = lingxi_core::types::ToolUseId::new();
            let uses = vec![(
                id.clone(),
                case["tool"].as_str().unwrap().into(),
                source(&case["coerced"]),
                None,
            )];
            let (blocks, _, _, _) =
                crate::turn_loop::dispatch_tool_uses_tracked(&orch, &uses, None)
                    .await
                    .unwrap();
            let lingxi_core::types::ContentBlock::ToolResult {
                content, is_error, ..
            } = &blocks[0]
            else {
                panic!("expected tool result")
            };
            assert!(is_error.unwrap_or(false));
            assert_eq!(
                content.string_at(""),
                Some(
                    format!(
                        "<tool_use_error>InputValidationError: {}</tool_use_error>",
                        case["display"].as_str().unwrap()
                    )
                    .as_str()
                ),
                "model {index}"
            );
            let persisted_results = orch.transcript.tool_use_results.lock().await;
            let persisted = persisted_results
                .get(&id.to_string())
                .and_then(|value| value.string_at(""));
            let expected = format!("InputValidationError: {}", case["raw"].as_str().unwrap());
            assert_eq!(persisted, Some(expected.as_str()), "persisted {index}");
        }
    }

    #[tokio::test]
    async fn monitor_main_dispatch_matches_native_270_schema_corpus() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;
        telemetry::test_set_flag("tengu_amber_sentinel", true);
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/task/tests/fixtures/monitor_schema_2_1_270.json"
        ))
        .unwrap();
        let context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });

        let tool: Arc<dyn tool_api::Tool> = Arc::new(tool_task::monitor::MonitorTool::new(context));
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(tool.clone());
        let orch = crate::conversation::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::path::PathBuf::from("/tmp"),
        );
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            telemetry::test_set_flag("tengu_breezy_crescent", case["mode"] == "bounded");
            if case["success"] == true {
                assert_eq!(
                    tool.parse_native_input(&source(&case["input"]))
                        .unwrap()
                        .unwrap(),
                    source(&case["normalized"]),
                    "parsed {index}"
                );
                assert!(
                    validate_tool_schema_detailed(tool.as_ref(), &source(&case["input"])).is_ok()
                );
                continue;
            }
            let id = lingxi_core::types::ToolUseId::new();
            let uses = vec![(id.clone(), "Monitor".into(), source(&case["input"]), None)];
            let (blocks, _, _, _) =
                crate::turn_loop::dispatch_tool_uses_tracked(&orch, &uses, None)
                    .await
                    .unwrap();
            let lingxi_core::types::ContentBlock::ToolResult {
                content, is_error, ..
            } = &blocks[0]
            else {
                panic!("expected tool result")
            };
            assert!(is_error.unwrap_or(false));
            assert_eq!(
                content.string_at(""),
                Some(
                    format!(
                        "<tool_use_error>InputValidationError: {}</tool_use_error>",
                        case["display"].as_str().unwrap()
                    )
                    .as_str()
                ),
                "model {index}"
            );
            assert_eq!(
                orch.transcript
                    .tool_use_results
                    .lock()
                    .await
                    .get(&id.to_string())
                    .and_then(|value| value.string_at("")),
                Some(format!("InputValidationError: {}", case["raw"].as_str().unwrap()).as_str()),
                "raw {index}"
            );
        }
        telemetry::test_clear_flag("tengu_amber_sentinel");
        telemetry::test_clear_flag("tengu_breezy_crescent");
    }

    #[test]
    fn issue_json_uses_javascript_numbers_and_integer_key_order() {
        assert_eq!(
            super::js_json(&json!([1.0, -0.0, 1e-7, 1e-6, 1e20, 1e21]), false),
            "[1,0,1e-7,0.000001,100000000000000000000,1e+21]"
        );
        assert_eq!(validate_rich_input_schema("Tool", &json!({"type":"object","properties":{},"additionalProperties":false}), &json!({"10":1,"2":1,"01":1})).unwrap_err(), "Tool failed due to the following issues:\nAn unexpected parameter `2` was provided\nAn unexpected parameter `10` was provided\nAn unexpected parameter `01` was provided");
        assert!(validate_rich_input_schema("Tool", &json!({"const":1}), &json!(1.0)).is_ok());
    }

    #[test]
    fn diagnostics_match_executed_2_1_263_zod_4_4_3_oracle() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/schema-validation-2.1.263.json"))
                .unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let actual = validate_rich_input_schema("Tool", &case["schema"], &case["input"]);
            let expected = case["expected"]
                .as_str()
                .map_or(Ok(()), |s| Err(s.to_owned()));
            assert_eq!(actual, expected, "oracle case {}", case["name"]);
        }
    }

    #[test]
    fn named_error_matches_oracle_grouping_and_bytes() {
        let schema = json!({"type":"object", "properties": {
            "a": {"type":"number"}, "path": {"type":"string"}
        }, "required":["path"], "additionalProperties":false});
        assert_eq!(
            validate_rich_input_schema("Read", &schema, &json!({"a":true,"extra":1}))
                .unwrap_err(),
            "Read failed due to the following issues:\nThe required parameter `path` is missing\nAn unexpected parameter `extra` was provided\nThe parameter `a` type is expected as `number` but provided as `boolean`"
        );
    }

    #[test]
    fn named_error_uses_zod_array_path_and_singular_prefix() {
        let schema = json!({"type":"object", "properties":{"rows":{"type":"array", "items":{"type":"object", "properties":{"name":{"type":"string"}}, "required":["name"]}}}});
        assert_eq!(
            validate_rich_input_schema("Tool", &schema, &json!({"rows":[{}]})).unwrap_err(),
            "Tool failed due to the following issue:\nThe required parameter `rows[0].name` is missing"
        );
    }

    #[test]
    fn enum_error_preserves_zod_issue_bytes() {
        let schema = json!({"type":"string", "enum":["a","b"]});
        let expected = json!([{"code":"invalid_value","values":["a","b"],"path":[],"message":"Invalid option: expected one of \"a\"|\"b\""}]);
        assert_eq!(
            validate_rich_input_schema("Tool", &schema, &json!("c")).unwrap_err(),
            serde_json::to_string_pretty(&expected).unwrap()
        );
    }

    #[test]
    fn string_constraints_count_utf16_code_units() {
        assert!(validate_rich_input_schema(
            "Tool",
            &json!({"type":"string","minLength":2}),
            &json!("😀")
        )
        .is_ok());
        let error = validate_rich_input_schema(
            "Tool",
            &json!({"type":"string","maxLength":1}),
            &json!("😀"),
        )
        .unwrap_err();
        let issues: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(issues[0]["code"], "too_big");
    }

    #[test]
    fn structural_errors_win_over_enum_issues_and_keep_declaration_order() {
        let schema = json!({"type":"object", "properties": {
            "z": {"type":"string"}, "a":{"type":"string"}, "mode":{"type":"string", "enum":["a"]}
        }, "required":["z","a","mode"]});
        assert_eq!(validate_rich_input_schema("Tool", &schema, &json!({"mode":"bad"})).unwrap_err(),
            "Tool failed due to the following issues:\nThe required parameter `z` is missing\nThe required parameter `a` is missing");
    }

    #[test]
    fn union_returns_nested_zod_errors_and_prefers_dirty_branch() {
        let schema = json!({"anyOf":[{"type":"string","minLength":5},{"type":"number"}]});
        let error = validate_rich_input_schema("Tool", &schema, &json!("a")).unwrap_err();
        let issues: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(issues[0]["code"], "too_small");
        let error = validate_rich_input_schema("Tool", &schema, &json!(true)).unwrap_err();
        let issues: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(issues[0]["code"], "invalid_union");
        assert_eq!(issues[0]["errors"][0][0]["code"], "invalid_type");
    }

    #[test]
    fn native_custom_issues_and_type_message_overrides_follow_zue() {
        let issue = json!({"code":"custom","path":[],"message":"Duplicate questions"});
        assert_eq!(
            super::format_issues("Tool", std::slice::from_ref(&issue)),
            serde_json::to_string_pretty(&vec![issue]).unwrap()
        );
        let issue = json!({"code":"invalid_type","expected":"string","received":"number","path":["field"],"message":"custom message"});
        assert_eq!(super::format_issues("Tool", &[issue]), "Tool failed due to the following issue:\nThe parameter `field` type is expected as `string` but provided as `unknown`");
    }

    #[test]
    fn missing_required_field_fails() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        let err = validate_tool_input_schema(&schema, &json!({})).unwrap_err();
        assert!(err.contains("path"), "message should name the field: {err}");
    }

    #[test]
    fn type_mismatch_fails() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        let err = validate_tool_input_schema(&schema, &json!({ "path": 123 })).unwrap_err();
        assert!(
            err.contains("/path"),
            "message should locate the field: {err}"
        );
    }

    #[test]
    fn valid_input_passes() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        assert!(validate_tool_input_schema(&schema, &json!({ "path": "/x" })).is_ok());
    }

    #[test]
    fn malformed_schema_passes_through() {
        // A JSON scalar is not a valid Draft-07 schema (a schema must be an
        // object or boolean), so boon fails to compile it and we take the
        // warn-and-PASS branch. A schema bug must never block the model's call.
        let schema = json!("not a schema object at all");
        assert!(validate_tool_input_schema(&schema, &json!({ "anything": true })).is_ok());
    }

    #[test]
    fn invalid_type_keyword_schema_passes_through() {
        // `type` must be a string or array of strings; `123` makes the schema
        // itself invalid, forcing boon's compile-error branch → warn + PASS.
        let schema = json!({ "type": 123 });
        assert!(validate_tool_input_schema(&schema, &json!({ "x": 1 })).is_ok());
    }

    #[test]
    fn empty_object_schema_accepts_anything() {
        let schema = json!({ "type": "object" });
        assert!(validate_tool_input_schema(&schema, &json!({ "a": 1, "b": [2] })).is_ok());
    }

    #[test]
    fn output_schema_mismatch_returns_detail() {
        // #38: a hook `updatedToolOutput` that does not match the tool's output
        // schema must FAIL so the caller keeps the original output and emits the
        // `... does not match <tool>'s output shape ...` meta message.
        let schema = json!({
            "type": "object",
            "properties": { "result": { "type": "string" } },
            "required": ["result"],
        });
        let err = validate_tool_output_schema(&schema, &json!({})).unwrap_err();
        assert!(
            err.contains("result"),
            "message should name the field: {err}"
        );
    }

    #[test]
    fn output_schema_match_passes() {
        let schema = json!({
            "type": "object",
            "properties": { "result": { "type": "string" } },
            "required": ["result"],
        });
        assert!(validate_tool_output_schema(&schema, &json!({ "result": "ok" })).is_ok());
    }

    #[test]
    fn output_schema_malformed_passes_through() {
        // A schema bug must not discard a hook's valid replacement.
        let schema = json!("not a schema");
        assert!(validate_tool_output_schema(&schema, &json!({ "x": 1 })).is_ok());
    }
}
