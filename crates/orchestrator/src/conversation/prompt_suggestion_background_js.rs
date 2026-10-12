use serde_json::{Value, json};
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
    hooks::function_hook::Sandbox::default()
        .eval_abandonable(&source, &input)
        .map(|result| result.value)
}
