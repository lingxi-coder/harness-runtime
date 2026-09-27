//! Shared native Zod diagnostics for main and nested tool dispatch.
//!
//! JSON rendering and issue grouping are executed-oracle locked to Claude Code
//! 2.1.270. The flat-schema collector covers the cron tools without adding a
//! validator dependency to this low-level crate; richer schemas stay with the
//! orchestrator's existing collector.
#![allow(missing_docs)]
use serde_json::{json, Value};

#[derive(Debug)]
pub struct NativeSchemaError {
    pub display: String,
    pub raw: String,
}

pub fn js_keys(object: &serde_json::Map<String, serde_json::Value>) -> Vec<&String> {
    fn index(key: &str) -> Option<u32> {
        let n = key.parse::<u32>().ok()?;
        (n != u32::MAX && n.to_string() == key).then_some(n)
    }
    let mut keys: Vec<_> = object.keys().collect();
    keys.sort_by_key(|key| index(key).map_or((1, 0), |n| (0, n)));
    keys
}

pub fn js_number(value: &serde_json::Number) -> String {
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
pub fn js_json(value: &serde_json::Value, pretty: bool) -> String {
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

pub fn js_value_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    if left.is_number() && right.is_number() {
        left.as_f64() == right.as_f64()
    } else {
        left == right
    }
}

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

pub fn format_issues(name: &str, issues: &[serde_json::Value]) -> String {
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

pub fn value_type(value: Option<&serde_json::Value>) -> &'static str {
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

pub fn quoted_options(values: &[serde_json::Value], separator: &str) -> String {
    values
        .iter()
        .map(|value| js_json(value, false))
        .collect::<Vec<_>>()
        .join(separator)
}

/// Zod object parsing ignores the unsafe prototype key rather than forwarding it.
pub fn normalize_flat_input(input: &Value) -> Option<Value> {
    if !input.as_object()?.contains_key("__proto__") {
        return None;
    }
    let mut normalized = input.clone();
    normalized.as_object_mut().unwrap().remove("__proto__");
    Some(normalized)
}

/// Return None for schema vocabulary outside this narrowly shared collector.
pub fn collect_flat_issues(schema: &Value, input: &Value) -> Option<Vec<Value>> {
    if schema["type"] != "object" || schema["additionalProperties"] != false {
        return None;
    }
    let properties = schema["properties"].as_object()?;
    if schema.as_object()?.keys().any(|key| {
        !matches!(
            key.as_str(),
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
    if properties.values().any(|s| {
        !matches!(s["type"].as_str(), Some("string" | "number" | "boolean"))
            || s.as_object().is_none_or(|s| {
                s.keys()
                    .any(|key| !matches!(key.as_str(), "type" | "description" | "title"))
            })
    }) {
        return None;
    }
    let mut issues = Vec::new();
    let type_issue = |expected: &str, value: Option<&Value>, path: Vec<Value>| json!({"expected":expected,"code":"invalid_type","path":path,"message":format!("Invalid input: expected {expected}, received {}", value_type(value))});
    let Some(object) = input.as_object() else {
        issues.push(type_issue("object", Some(input), vec![]));
        return Some(issues);
    };
    for key in js_keys(properties) {
        let value = object.get(key);
        if value.is_none()
            && !schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|v| v.as_str() == Some(key)))
        {
            continue;
        }
        let expected = properties[key]["type"].as_str().unwrap();
        if value_type(value) != expected {
            issues.push(type_issue(expected, value, vec![json!(key)]));
        }
    }
    let keys: Vec<_> = js_keys(object)
        .into_iter()
        .filter(|key| key.as_str() != "__proto__" && !properties.contains_key(*key))
        .cloned()
        .collect();
    if !keys.is_empty() {
        issues.push(json!({"code":"unrecognized_keys","keys":keys,"path":[],"message":format!("Unrecognized key{}: {}", if keys.len()==1 {""} else {"s"}, keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "))}));
    }
    Some(issues)
}

pub fn validate_flat_input(
    name: &str,
    schema: &Value,
    input: &Value,
) -> Option<Result<(), NativeSchemaError>> {
    let issues = collect_flat_issues(schema, input)?;
    Some(if issues.is_empty() {
        Ok(())
    } else {
        Err(NativeSchemaError {
            display: format_issues(name, &issues),
            raw: js_json(&Value::Array(issues), true),
        })
    })
}
