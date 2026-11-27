use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;

fn object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("background input has distinct static field names")
}

pub(super) enum ClassifierContent {
    Text(String),
    TextUtf16(Vec<u16>),
    Provider(Utf16JsonProjection),
    Tool(String),
}

impl ClassifierContent {
    fn capture(block: &ContentBlock) -> Option<Self> {
        match block {
            ContentBlock::Text { text, .. } => Some(Self::Text(text.clone())),
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => Some(Self::TextUtf16(utf16_code_units.clone())),
            ContentBlock::ProviderContent { protocol, value }
                if protocol == "anthropic_messages"
                    && matches!(value.string_at("/type"), Some("text" | "tool_use")) =>
            {
                Some(Self::Provider(value.clone()))
            }
            ContentBlock::ToolUse { name, .. } => Some(Self::Tool(name.clone())),
            _ => None,
        }
    }

    fn into_native(self) -> Utf16JsonProjection {
        let text = match self {
            Self::Text(text) => text.into(),
            Self::TextUtf16(units) => {
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                    .expect("assistant text is a source string")
            }
            Self::Provider(value) => return value,
            Self::Tool(name) => {
                return object(vec![
                    ("type".into(), "tool_use".into()),
                    ("name".into(), name.into()),
                ])
            }
        };
        object(vec![("type".into(), "text".into()), ("text".into(), text)])
    }
}

/// Capture only the fields the classifier reads while the transcript is locked.
pub(super) fn capture_assistant(
    message: &ConversationMessage,
) -> Option<(MessageId, Vec<ClassifierContent>)> {
    let ConversationMessage::Assistant { id, content, .. } = message else {
        return None;
    };
    Some((
        *id,
        content
            .iter()
            .filter_map(ClassifierContent::capture)
            .collect(),
    ))
}

#[cfg(test)]
#[allow(dead_code)] // Also included directly by the integration fixtures.
pub(super) fn assemble_history(
    history: &[ConversationMessage],
    last_count: usize,
    is_api_error: impl Fn(&MessageId) -> bool,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    let assistants = history
        .iter()
        .filter_map(capture_assistant)
        .map(|(id, content)| (is_api_error(&id), content));
    assemble_assistants(assistants, last_count)
}

pub(super) fn assemble_assistants(
    assistants: impl IntoIterator<Item = (bool, Vec<ClassifierContent>)>,
    last_count: usize,
) -> Result<Utf16JsonProjection, hooks::function_hook::FunctionHookError> {
    let messages = assistants
        .into_iter()
        .map(|(is_api_error, content)| {
            let content = Utf16JsonProjection::array(
                content
                    .into_iter()
                    .map(ClassifierContent::into_native)
                    .collect(),
            )
            .expect("native classifier content is an array");
            object(vec![
                ("type".into(), "assistant".into()),
                ("message".into(), object(vec![("content".into(), content)])),
                (
                    "isApiErrorMessage".into(),
                    Utf16JsonProjection::from_serde_value(&json!(is_api_error))
                        .expect("API error flag is a JSON boolean"),
                ),
            ])
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
    const SOURCE: &str = concat!(
        include_str!("prompt_suggestion_redactor_native.js"),
        "\nconst classifier = (() => {\n",
        include_str!("prompt_suggestion_classifier_adapter.js"),
        "\n",
        include_str!("prompt_suggestion_classifier_native.js"),
        "\nreturn {wZr,Oxn,Hxn,vZr,brs,wrs};\n})();\n",
        include_str!("prompt_suggestion_background_tail_native.js"),
        "\n",
        include_str!("prompt_suggestion_background_input_native.js"),
        "\n",
        include_str!("prompt_suggestion_background_adapter.js"),
    );
    let result = hooks::function_hook::Sandbox::default().eval_abandonable(SOURCE, input)?;
    Ok(result)
}
