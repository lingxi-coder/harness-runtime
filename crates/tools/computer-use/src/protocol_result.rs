//! MCP result projection shared by the separately registered computer tools.
use super::{json, ToolCallResult, Utf16JsonProjection, Value};

pub(super) fn from_wire(wire: Value) -> ToolCallResult {
    let content = wire.get("content").cloned().unwrap_or_else(|| json!([]));
    let texts = content.as_array().and_then(|blocks| {
        blocks
            .iter()
            .map(|block| {
                (block["type"] == "text")
                    .then(|| block["text"].as_str())
                    .flatten()
            })
            .collect::<Option<Vec<_>>>()
    });
    let model_content = texts.map(|parts| Utf16JsonProjection::from(parts.join("\n")));
    let mut result =
        ToolCallResult::from_data(tool_api::tool_trait::generated_builtin_result(&content));
    result.model_content = model_content;
    result.is_error = wire
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    result
}
pub(super) fn text(message: &str) -> ToolCallResult {
    from_wire(json!({"content":[{"type":"text","text":message}]}))
}
pub(super) fn error(message: &str, kind: &str) -> ToolCallResult {
    from_wire(
        json!({"content":[{"type":"text","text":message}],"isError":true,"telemetry":{"error_kind":kind}}),
    )
}
pub(super) fn error_source(message: Utf16JsonProjection) -> ToolCallResult {
    let mut content =
        Utf16JsonProjection::parse(r#"[{"type":"text","text":""}]"#).expect("literal content");
    content
        .replace_pointer_projection("/0/text", message.clone())
        .expect("literal text field");
    let mut result = ToolCallResult::from_data(content);
    result.model_content = Some(message);
    result.is_error = true;
    result
}
pub(super) fn json(value: Value) -> ToolCallResult {
    text(&serde_json::to_string(&value).expect("generated protocol value is serializable"))
}
pub(super) fn wire(result: &ToolCallResult) -> Value {
    let content =
        serde_json::from_str(&result.data.to_json_string().unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|_| json!([]));
    let mut value = json!({"content":content});
    if result.is_error {
        value["isError"] = Value::Bool(true);
    }
    value
}
