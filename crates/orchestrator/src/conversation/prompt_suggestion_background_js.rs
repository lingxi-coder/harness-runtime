use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
pub(super) fn evaluate(operation: &str, input: Value) -> Option<Value> {
    evaluate_result(operation, input).ok()
}

pub(super) fn evaluate_result(
    operation: &str,
    mut input: Value,
) -> Result<Value, hooks::function_hook::FunctionHookError> {
    input["operation"] = json!(operation);
    let source = format!(
        "{}\nconst classifier = (() => {{\n{}\n{}\nreturn {{wZr,Oxn,Hxn,vZr,brs,wrs}};\n}})();\n{}\n{}\n{}",
        include_str!("prompt_suggestion_redactor_native.js"),
        include_str!("prompt_suggestion_classifier_adapter.js"),
        include_str!("prompt_suggestion_classifier_native.js"),
        include_str!("prompt_suggestion_background_tail_native.js"),
        include_str!("prompt_suggestion_background_input_native.js"),
        include_str!("prompt_suggestion_background_adapter.js"),
    );
    let input = Utf16JsonProjection::from_serde_value(&input).map_err(|error| {
        hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string())
    })?;
    let result = hooks::function_hook::Sandbox::default().eval_abandonable(&source, &input)?;
    let result_source = result.to_json_string().map_err(|error| {
        hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string())
    })?;
    serde_json::from_str(&result_source).map_err(|error| {
        hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string())
    })
}
