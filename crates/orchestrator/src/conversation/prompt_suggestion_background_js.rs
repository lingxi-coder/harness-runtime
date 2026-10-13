use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
pub(super) fn evaluate(operation: &str, input: Value) -> Option<Utf16JsonProjection> {
    evaluate_result(operation, input).ok()
}

pub(super) fn evaluate_parse_projection(
    text: Utf16JsonProjection,
    previous: &str,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    let input = Utf16JsonProjection::object(vec![
        ("text".into(), text),
        ("previous".into(), Utf16JsonProjection::from(previous)),
    ])
    .map_err(|error| hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string()))?;
    evaluate_projection_result("parse", input)
}

pub(super) fn evaluate_projection_result(
    operation: &str,
    mut input: Utf16JsonProjection,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    input
        .set_field("operation", Utf16JsonProjection::from(operation))
        .map_err(|error| {
            hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string())
        })?;
    evaluate_projection(&input)
}

pub(super) fn evaluate_result(
    operation: &str,
    mut input: Value,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    input["operation"] = json!(operation);
    let input = Utf16JsonProjection::from_serde_value(&input).map_err(|error| {
        hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string())
    })?;
    evaluate_projection(&input)
}

fn evaluate_projection(
    input: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    let source = format!(
        "{}\nconst classifier = (() => {{\n{}\n{}\nreturn {{wZr,Oxn,Hxn,vZr,brs,wrs}};\n}})();\n{}\n{}\n{}",
        include_str!("prompt_suggestion_redactor_native.js"),
        include_str!("prompt_suggestion_classifier_adapter.js"),
        include_str!("prompt_suggestion_classifier_native.js"),
        include_str!("prompt_suggestion_background_tail_native.js"),
        include_str!("prompt_suggestion_background_input_native.js"),
        include_str!("prompt_suggestion_background_adapter.js"),
    );
    let result = hooks::function_hook::Sandbox::default().eval_abandonable(&source, input)?;
    Ok(result)
}
