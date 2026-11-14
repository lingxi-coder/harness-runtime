use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;

fn object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("background input has distinct static field names")
}

fn native_content(block: &ContentBlock) -> Option<Utf16JsonProjection> {
    let text = match block {
        ContentBlock::Text { text, .. } => Utf16JsonProjection::from(text.clone()),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Utf16JsonProjection::root_string(
            String::from_utf16_lossy(utf16_code_units),
            utf16_code_units.clone(),
        )
        .expect("assistant text is a source string"),
        ContentBlock::ProviderContent { protocol, value }
            if protocol == "anthropic_messages"
                && matches!(value.string_at("/type"), Some("text" | "tool_use")) =>
        {
            return Some(value.clone());
        }
        ContentBlock::ToolUse { name, .. } => {
            return Some(object(vec![
                ("type".into(), "tool_use".into()),
                ("name".into(), name.clone().into()),
            ]));
        }
        _ => return None,
    };
    Some(object(vec![
        ("type".into(), "text".into()),
        ("text".into(), text),
    ]))
}

/// Project history to the native classifier's message shape, preserving source
/// strings instead of serializing the host's internal content-block tags.
pub(super) fn assemble_history(
    history: &[ConversationMessage],
    last_count: usize,
    is_api_error: impl Fn(&MessageId) -> bool,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    let messages = history
        .iter()
        .filter_map(|message| {
            let ConversationMessage::Assistant { id, content, .. } = message else {
                return None;
            };
            let content =
                Utf16JsonProjection::array(content.iter().filter_map(native_content).collect())
                    .expect("native classifier content is an array");
            Some(object(vec![
                ("type".into(), "assistant".into()),
                ("message".into(), object(vec![("content".into(), content)])),
                (
                    "isApiErrorMessage".into(),
                    Utf16JsonProjection::from_serde_value(&json!(is_api_error(id)))
                        .expect("API error flag is a JSON boolean"),
                ),
            ]))
        })
        .collect();
    evaluate_projection_result(
        "assemble",
        object(vec![
            (
                "messages".into(),
                Utf16JsonProjection::array(messages).expect("classifier messages are an array"),
            ),
            (
                "lastCount".into(),
                Utf16JsonProjection::from_serde_value(&json!(last_count))
                    .expect("assistant count is a JSON number"),
            ),
        ]),
    )
}

pub(super) fn prepare_assembled(
    assembled: &Utf16JsonProjection,
    previous: &str,
    latest_ask: Utf16JsonProjection,
    minutes: f64,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    let field = |pointer| {
        assembled.subprojection(pointer).map_err(|error| {
            hooks::function_hook::FunctionHookError::NotSerialisable(error.to_string())
        })
    };
    evaluate_projection_result(
        "prepare",
        object(vec![
            ("text".into(), field("/text")?),
            ("previous".into(), previous.into()),
            ("latestAsk".into(), latest_ask),
            ("tools".into(), field("/tools")?),
            (
                "minutes".into(),
                Utf16JsonProjection::from_serde_value(&json!(minutes))
                    .expect("elapsed minutes is a JSON number"),
            ),
        ]),
    )
}

#[cfg(test)]
// Called by tests/prompt_suggestion_background.rs, which includes this file.
#[allow(dead_code)]
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

#[cfg(test)]
// Called by tests/prompt_suggestion_background.rs, which includes this file.
#[allow(dead_code)]
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
    static SOURCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
        "{}\nconst classifier = (() => {{\n{}\n{}\nreturn {{wZr,Oxn,Hxn,vZr,brs,wrs}};\n}})();\n{}\n{}\n{}",
        include_str!("prompt_suggestion_redactor_native.js"),
        include_str!("prompt_suggestion_classifier_adapter.js"),
        include_str!("prompt_suggestion_classifier_native.js"),
        include_str!("prompt_suggestion_background_tail_native.js"),
        include_str!("prompt_suggestion_background_input_native.js"),
        include_str!("prompt_suggestion_background_adapter.js"),
    )
    });
    let result = hooks::function_hook::Sandbox::default().eval_abandonable(&SOURCE, input)?;
    Ok(result)
}
