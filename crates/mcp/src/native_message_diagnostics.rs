//! Zod issue ordering for the four strict native MCP envelope branches.
use jsonrpc::JsonRpcString;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::{json, Value};

enum Diagnostic {
    Json(Value),
    Text(JsonRpcString),
    Array(Vec<Self>),
    Object(Vec<(&'static str, Self)>),
}
impl Serialize for Diagnostic {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Json(value) => value.serialize(serializer),
            Self::Text(value) => value.serialize(serializer),
            Self::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}
impl From<Value> for Diagnostic {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}
#[derive(Default)]
struct Issues {
    values: Vec<Diagnostic>,
    aborted: bool,
}
impl Issues {
    fn append(&mut self, mut other: Self) {
        self.aborted |= other.aborted;
        self.values.append(&mut other.values);
    }
    fn issue(value: Value, aborted: bool) -> Self {
        Self {
            values: vec![value.into()],
            aborted,
        }
    }
}
fn received(value: Option<JsonValueView<'_>>) -> &'static str {
    let Some(value) = value else {
        return "undefined";
    };
    if value.is_null() {
        "null"
    } else if value.code_units().is_some() {
        "string"
    } else if value.as_number().is_some() {
        "number"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.array_len().is_some() {
        "array"
    } else {
        "object"
    }
}
fn invalid(expected: &str, value: Option<JsonValueView<'_>>, path: &[&str]) -> Issues {
    Issues::issue(
        json!({"expected":expected,"code":"invalid_type","path":path,"message":format!("Invalid input: expected {expected}, received {}",received(value))}),
        true,
    )
}
fn integer(value: Option<JsonValueView<'_>>, path: &[&str]) -> Issues {
    let Some(number) = value.and_then(JsonValueView::as_number) else {
        return invalid("number", value, path);
    };
    if !number.is_finite() {
        return invalid("number", value, path);
    }
    if number.fract() != 0.0 {
        return Issues::issue(
            json!({"expected":"int","format":"safeint","code":"invalid_type","path":path,"message":"Invalid input: expected int, received number"}),
            true,
        );
    }
    const MAX: i64 = 9_007_199_254_740_991;
    if number.abs() > MAX as f64 {
        return if number > 0.0 {
            Issues::issue(
                json!({"code":"too_big","maximum":MAX,"note":"Integers must be within the safe integer range.","origin":"int","inclusive":true,"path":path,"message":format!("Too big: expected int to be <={MAX}")}),
                false,
            )
        } else {
            Issues::issue(
                json!({"code":"too_small","minimum":-MAX,"note":"Integers must be within the safe integer range.","origin":"int","inclusive":true,"path":path,"message":format!("Too small: expected int to be >=-{}",MAX)}),
                false,
            )
        };
    }
    Issues::default()
}
fn union(mut branches: Vec<Issues>, path: &[&str]) -> Issues {
    if branches.iter().any(|branch| branch.values.is_empty()) {
        return Issues::default();
    }
    let viable: Vec<_> = branches
        .iter()
        .enumerate()
        .filter_map(|(index, branch)| (!branch.aborted).then_some(index))
        .collect();
    if viable.len() == 1 {
        return branches.swap_remove(viable[0]);
    }
    Issues {
        values: vec![Diagnostic::Object(vec![
            ("code", json!("invalid_union").into()),
            (
                "errors",
                Diagnostic::Array(
                    branches
                        .into_iter()
                        .map(|branch| Diagnostic::Array(branch.values))
                        .collect(),
                ),
            ),
            ("path", json!(path).into()),
            ("message", json!("Invalid input").into()),
        ])],
        aborted: true,
    }
}
fn id(value: Option<JsonValueView<'_>>, path: &[&str]) -> Issues {
    if value.is_some_and(|value| value.code_units().is_some()) {
        return Issues::default();
    }
    let string = invalid("string", value, &[]);
    let number = integer(value, &[]);
    let mut result = union(vec![string, number], path);
    // A successful number branch has no issue; a unique continuable numeric
    // bound issue inherits the containing path rather than a union wrapper.
    if !result.aborted && !result.values.is_empty() {
        result = integer(value, path);
    }
    result
}
fn string(value: Option<JsonValueView<'_>>, path: &[&str]) -> Issues {
    if value.is_some_and(|value| value.code_units().is_some()) {
        Issues::default()
    } else {
        invalid("string", value, path)
    }
}
fn object(value: Option<JsonValueView<'_>>, path: &[&str]) -> Issues {
    if value.is_some_and(|value| value.object_entries().is_some()) {
        Issues::default()
    } else {
        invalid("object", value, path)
    }
}
fn params(value: JsonValueView<'_>) -> Issues {
    let mut issues = Issues::default();
    let Some(params) = value.get_property("params") else {
        return issues;
    };
    issues.append(object(Some(params), &["params"]));
    if issues.aborted {
        return issues;
    }
    let Some(meta) = params.get_property("_meta") else {
        return issues;
    };
    issues.append(object(Some(meta), &["params", "_meta"]));
    if issues.aborted {
        return issues;
    }
    if let Some(token) = meta.get_property("progressToken") {
        issues.append(id(Some(token), &["params", "_meta", "progressToken"]));
    }
    if let Some(task) = meta.get_property("io.modelcontextprotocol/related-task") {
        let shape = object(
            Some(task),
            &["params", "_meta", "io.modelcontextprotocol/related-task"],
        );
        if !shape.aborted {
            issues.append(string(
                task.get_property("taskId"),
                &[
                    "params",
                    "_meta",
                    "io.modelcontextprotocol/related-task",
                    "taskId",
                ],
            ));
        }
        issues.append(shape);
    }
    issues
}
fn strict(value: JsonValueView<'_>, allowed: &[&str]) -> Issues {
    let keys: Vec<_> = value
        .object_entries()
        .unwrap_or_default()
        .iter()
        .filter(|entry| {
            !allowed
                .iter()
                .any(|key| entry.code_units.iter().copied().eq(key.encode_utf16()))
        })
        .map(|entry| entry.code_units.to_vec())
        .collect();
    if keys.is_empty() {
        return Issues::default();
    }
    let mut message = if keys.len() == 1 {
        "Unrecognized key: "
    } else {
        "Unrecognized keys: "
    }
    .encode_utf16()
    .collect::<Vec<_>>();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            message.extend(", ".encode_utf16());
        }
        message.push(b'"' as u16);
        message.extend_from_slice(key);
        message.push(b'"' as u16);
    }
    Issues {
        values: vec![Diagnostic::Object(vec![
            ("code", json!("unrecognized_keys").into()),
            (
                "keys",
                Diagnostic::Array(
                    keys.into_iter()
                        .map(|key| Diagnostic::Text(JsonRpcString::from_code_units(key)))
                        .collect(),
                ),
            ),
            ("path", json!([]).into()),
            (
                "message",
                Diagnostic::Text(JsonRpcString::from_code_units(message)),
            ),
        ])],
        aborted: false,
    }
}
fn branch(value: JsonValueView<'_>, kind: u8) -> Issues {
    let mut issues = object(Some(value), &[]);
    if issues.aborted {
        return issues;
    }
    if !value.get_property("jsonrpc").is_some_and(|value| {
        value
            .code_units()
            .is_some_and(|units| units.iter().copied().eq("2.0".encode_utf16()))
    }) {
        issues.append(Issues::issue(json!({"code":"invalid_value","values":["2.0"],"path":["jsonrpc"],"message":"Invalid input: expected \"2.0\""}),true));
    }
    if kind != 1 && (kind != 3 || value.get_property("id").is_some()) {
        issues.append(id(value.get_property("id"), &["id"]));
    }
    let allowed: &[&str] = match kind {
        0 | 1 => {
            issues.append(string(value.get_property("method"), &["method"]));
            issues.append(params(value));
            if kind == 0 {
                &["jsonrpc", "id", "method", "params"]
            } else {
                &["jsonrpc", "method", "params"]
            }
        }
        2 => {
            let result = value.get_property("result");
            let shape = object(result, &["result"]);
            if !shape.aborted {
                if let Some(meta) = result.and_then(|result| result.get_property("_meta")) {
                    issues.append(object(Some(meta), &["result", "_meta"]));
                }
            }
            issues.append(shape);
            &["jsonrpc", "id", "result"]
        }
        _ => {
            let error = value.get_property("error");
            let shape = object(error, &["error"]);
            if !shape.aborted {
                let error = error.unwrap();
                issues.append(integer(error.get_property("code"), &["error", "code"]));
                issues.append(string(error.get_property("message"), &["error", "message"]));
            }
            issues.append(shape);
            &["jsonrpc", "id", "error"]
        }
    };
    issues.append(strict(value, allowed));
    issues
}

pub(super) fn schema_error(source: &str) -> Option<String> {
    let projection = Utf16JsonProjection::parse(source).ok()?;
    let value = projection.at_pointer("")?;
    let issues = union((0..4).map(|kind| branch(value, kind)).collect(), &[]);
    serde_json::to_string_pretty(&Diagnostic::Array(issues.values)).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_322_envelope_diagnostics_and_normalization_match_source_replay() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../platforms/common/tests/fixtures/mcp_remote_gap_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["envelopeCases"].as_array().unwrap() {
            let source = case["input"].as_str().unwrap();
            match super::super::decode_native_mcp_frame(source.as_bytes()) {
                Ok(message) => assert_eq!(
                    serde_json::to_string(&message).unwrap(),
                    case["output"].as_str().unwrap(),
                    "{source}"
                ),
                Err(jsonrpc::CodecError::MessageDecode(error)) => {
                    assert_eq!(error, case["error"].as_str().unwrap(), "{source}")
                }
                Err(error) => panic!("unexpected error for {source}: {error}"),
            }
        }
    }
}
