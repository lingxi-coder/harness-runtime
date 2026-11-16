//! Host-owned discovery receipts. Definitions are frozen when a ToolSearch
//! result is accepted, then projected only on an admitted Responses route.
use crate::{AuthStrategy, LlmError, LlmRequest};
use lingxi_core::types::{ContentBlock as HostBlock, ConversationMessage as HostMessage};
use lingxi_llm_client::{
    codecs::{CodecContext, EncodeRequest, WireCodec},
    protocol::{self as wire, JsonPayload},
    providers::openai::types::{OpenAiToolSearchConfig, OpenAiToolSearchExecution},
    RequestMode,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const CALL: &str = "lingxi_tool_search_call";
pub(crate) const DISCOVERY: &str = "lingxi_tool_discovery";

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct History {
    calls: BTreeMap<String, JsonPayload>,
    definitions: BTreeMap<String, JsonPayload>,
    legacy_names: BTreeSet<String>,
}

fn invalid(message: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest {
        message: message.to_string(),
    }
}

pub(crate) fn call(value: &JsonPayload) -> Option<(String, JsonPayload)> {
    if value.string_at("/type").as_deref() != Some("tool_search_call")
        || value.string_at("/execution").as_deref() != Some("client")
        || value
            .string_at("/status")
            .is_some_and(|status| status != "completed")
    {
        return None;
    }
    let id = value.string_at("/call_id")?;
    if id.is_empty()
        || id.chars().any(char::is_control)
        || String::from_utf16(&value.payload_at("/call_id")?.root_string_code_units()?).is_err()
    {
        return None;
    }
    let arguments = value
        .payload_at("/arguments")?
        .is_root_object()
        .then(|| value.payload_at("/arguments"))
        .flatten()?;
    Some((id, arguments))
}

/// Strip host-only carriers before generic normalization. Keeping their data
/// outside canonical model input also preserves it across protocol retries.
pub(crate) fn capture_history(messages: &mut [HostMessage]) -> Result<History, LlmError> {
    let mut history = History::default();
    let mut searches = BTreeSet::new();
    for message in messages.iter() {
        let blocks = match message {
            HostMessage::User { content, .. } | HostMessage::Assistant { content, .. } => content,
            _ => continue,
        };
        for block in blocks {
            if let HostBlock::ToolUse {
                id,
                name,
                provider_id,
                ..
            } = block
            {
                if name == "ToolSearch" {
                    searches.insert(provider_id.clone().unwrap_or_else(|| id.to_string()));
                }
            }
        }
    }
    for message in messages.iter() {
        let blocks = match message {
            HostMessage::User { content, .. } | HostMessage::Assistant { content, .. } => content,
            _ => continue,
        };
        for block in blocks {
            let HostBlock::ProviderContent { value, .. } = block else {
                continue;
            };
            if value.string_at("/type") == Some(CALL) {
                let native = value.subprojection("/call").map_err(invalid)?;
                let native = JsonPayload::parse(&native.to_json_string().map_err(invalid)?)
                    .map_err(invalid)?;
                let (id, _) =
                    call(&native).ok_or_else(|| invalid("invalid native ToolSearch receipt"))?;
                if searches.contains(&id) {
                    history.calls.insert(id, native);
                }
            } else if value.string_at("/type") == Some(DISCOVERY) {
                let id = value
                    .string_at("/call_id")
                    .ok_or_else(|| invalid("ToolSearch discovery has no call ID"))?;
                let definitions = value.subprojection("/definitions").map_err(invalid)?;
                if definitions.array_len("").is_none() {
                    return Err(invalid("ToolSearch discovery definitions must be an array"));
                }
                history.definitions.insert(
                    id.to_owned(),
                    JsonPayload::parse(&definitions.to_json_string().map_err(invalid)?)
                        .map_err(invalid)?,
                );
            }
        }
    }
    for message in messages.iter_mut() {
        let blocks = match message {
            HostMessage::User { content, .. } | HostMessage::Assistant { content, .. } => content,
            _ => continue,
        };
        for block in blocks.iter() {
            let HostBlock::ToolResult {
                tool_use_id,
                provider_tool_use_id,
                content_blocks: Some(parts),
                ..
            } = block
            else {
                continue;
            };
            let id = provider_tool_use_id
                .clone()
                .unwrap_or_else(|| tool_use_id.to_string());
            if searches.contains(&id) && !history.calls.contains_key(&id) {
                for index in 0..parts.array_len("").unwrap_or(0) {
                    if let Some(name) = parts.string_at(&format!("/{index}/tool_name")) {
                        history.legacy_names.insert(name.to_owned());
                    }
                }
            }
        }
        blocks.retain(|block| {
            !matches!(block, HostBlock::ProviderContent { value, .. }
            if matches!(value.string_at("/type"), Some(CALL | DISCOVERY)))
        });
    }
    Ok(history)
}

fn descriptor(
    request: &LlmRequest,
    profile: &wire::ProviderProfile,
    model: &str,
    auth: AuthStrategy,
) -> Option<wire::HostedTool> {
    if profile.provider_id.as_str() != "openai"
        || profile.protocol != wire::ProtocolFamily::OpenAiResponses
        || !matches!(auth, AuthStrategy::ApiKey | AuthStrategy::Bearer)
        || request.execution.local_tool_search_history.is_none()
        || request.input.hosted_openai_tool_search().is_some()
        || request.input.tool_choice != wire::ToolChoice::Auto
    {
        return None;
    }
    let search = request
        .input
        .tools
        .iter()
        .find(|tool| tool.name == "ToolSearch")?;
    let config = OpenAiToolSearchConfig {
        execution: OpenAiToolSearchExecution::Client,
        description: Some(String::from_utf16(&search.description.root_string_code_units()?).ok()?),
        parameters: Some(search.input_schema.clone()),
    };
    let tool: wire::HostedTool =
        lingxi_llm_client::providers::openai::native::OpenAiHostedTool::ToolSearch(config).into();
    let mut probe = wire::ChatRequest::new(model);
    probe
        .messages
        .push(wire::ConversationMessage::user_text("find a tool"));
    probe.hosted_tools.push(tool.clone());
    let mode = if request.stream {
        RequestMode::Stream
    } else {
        RequestMode::Complete
    };
    lingxi_llm_client::codecs::openai::responses::OpenAiResponsesCodec
        .encode_request(
            EncodeRequest::new(&probe),
            &CodecContext::new(profile, model, mode),
        )
        .ok()?;
    Some(tool)
}

/// The same top-level projection is used by dispatch and prompt fingerprints.
pub(crate) fn tools(
    request: &LlmRequest,
    profile: &wire::ProviderProfile,
    model: &str,
    auth: AuthStrategy,
) -> Option<(Vec<wire::ToolSpec>, Vec<wire::HostedTool>)> {
    let search = descriptor(request, profile, model, auth)?;
    let mut hosted = request.input.hosted_tools.clone();
    hosted.push(search);
    let legacy = request.execution.local_tool_search_history.as_ref();
    let tools = request
        .input
        .tools
        .iter()
        .filter(|tool| {
            tool.name != "ToolSearch"
                && (!tool.defer_loading
                    || legacy.is_some_and(|history| history.legacy_names.contains(&tool.name)))
        })
        .cloned()
        .map(|mut tool| {
            tool.defer_loading = false;
            tool
        })
        .collect();
    Some((tools, hosted))
}

fn function(definition: &JsonPayload) -> Result<JsonPayload, LlmError> {
    let mut output =
        JsonPayload::parse(r#"{"type":"function","defer_loading":true}"#).map_err(invalid)?;
    for (from, to) in [
        ("name", "name"),
        ("description", "description"),
        ("input_schema", "parameters"),
        ("strict", "strict"),
    ] {
        if let Some(value) = definition.payload_at(&format!("/{from}")) {
            output = output.with_object_field(to, &value).map_err(invalid)?;
        }
    }
    if output.string_at("/name").is_none()
        || !output
            .payload_at("/parameters")
            .is_some_and(|value| value.is_root_object())
    {
        return Err(invalid("invalid frozen ToolSearch function definition"));
    }
    if output.raw_source_at("/strict").as_deref() == Some("true") {
        let schema = output.payload_at("/parameters").unwrap();
        match lingxi_llm_client::providers::anthropic::strict_schema::to_strict_schema(&schema) {
            Ok(schema) => {
                output = output
                    .with_object_field("parameters", &schema)
                    .map_err(invalid)?
            }
            Err(_) => {
                output = output
                    .with_object_field("strict", &JsonPayload::parse("false").map_err(invalid)?)
                    .map_err(invalid)?
            }
        }
    }
    Ok(output)
}

pub(crate) fn project<'a>(
    request: &'a LlmRequest,
    profile: &wire::ProviderProfile,
    model: &str,
    auth: AuthStrategy,
) -> Result<std::borrow::Cow<'a, LlmRequest>, LlmError> {
    use std::borrow::Cow;
    let Some((tools, hosted)) = tools(request, profile, model, auth) else {
        return Ok(Cow::Borrowed(request));
    };
    let mut projected = request.clone();
    projected.input.tools = tools;
    projected.input.hosted_tools = hosted;
    projected.execution.local_tool_search_admitted = true;
    let Some(history) = request.execution.local_tool_search_history.as_ref() else {
        return Ok(Cow::Owned(projected));
    };
    let mut position_map = BTreeMap::new();
    let mut text_results = BTreeMap::new();
    for (mi, message) in projected.input.messages.iter_mut().enumerate() {
        let previous = std::mem::take(&mut message.content);
        for (bi, block) in previous.into_iter().enumerate() {
            let start = message.content.len();
            match block {
                wire::ContentBlock::ToolUse {
                    ref id, ref name, ..
                } if name == "ToolSearch" && history.calls.contains_key(id.as_str()) => {
                    message.content.push(wire::ContentBlock::ProviderContent {
                        protocol: wire::ProtocolFamily::OpenAiResponses,
                        value: history.calls[id.as_str()].clone(),
                    });
                }
                wire::ContentBlock::ToolResult {
                    ref tool_use_id,
                    ref content,
                    ref blocks,
                    ..
                } if history.calls.contains_key(tool_use_id.as_str()) => {
                    let definitions = history.definitions.get(tool_use_id.as_str());
                    let definitions = (0..definitions
                        .and_then(|value| value.array_len(""))
                        .unwrap_or(0))
                        .map(|index| function(&definitions.unwrap().array_item(index).unwrap()))
                        .collect::<Result<Vec<_>, _>>()?;
                    let output = JsonPayload::parse(r#"{"type":"tool_search_output","execution":"client","status":"completed"}"#)
                        .map_err(invalid)?
                        .with_object_field("call_id", &JsonPayload::string(tool_use_id.as_str())).map_err(invalid)?
                        .with_object_field("tools", &JsonPayload::array_from_items(&definitions,
                            lingxi_llm_client::exact_json::JsonEncoding::JavaScript).map_err(crate::upstream::error)?).map_err(invalid)?;
                    message.content.push(wire::ContentBlock::ProviderContent {
                        protocol: wire::ProtocolFamily::OpenAiResponses,
                        value: output,
                    });
                    let mut units = request
                        .execution
                        .message_json_string_overrides
                        .get(&format!("/messages/{mi}/content/{bi}/content"))
                        .cloned()
                        .or_else(|| content.root_string_code_units())
                        .unwrap_or_else(|| content.as_json_source().encode_utf16().collect());
                    if definitions.is_empty()
                        && blocks.as_ref().is_some_and(|parts| {
                            (0..parts.array_len("").unwrap_or(0)).any(|index| {
                                parts.array_item(index).is_some_and(|part| {
                                    part.string_at("/type").as_deref() == Some("tool_reference")
                                })
                            })
                        })
                    {
                        units.extend("\nThe tool catalog changed before definitions were published. Retry ToolSearch to load the current callable schemas.".encode_utf16());
                    }
                    text_results.insert((mi, bi), units.clone());
                    message.content.push(wire::ContentBlock::TextJsUtf16 {
                        text: String::from_utf16_lossy(&units),
                        utf16_code_units: units,
                        thought_signature: None,
                        citations: None,
                    });
                }
                block => message.content.push(block),
            }
            position_map.insert((mi, bi), (start, message.content.len() - 1));
        }
    }
    for point in &mut projected.input.prompt_cache.breakpoints {
        if let wire::CachePosition::Message { index, block } = &mut point.position {
            if let Some((_, last)) = position_map.get(&(*index, *block)) {
                *block = *last;
            }
        }
    }
    let mut exact = BTreeMap::new();
    for (key, units) in &request.execution.message_json_string_overrides {
        let Some(path) = key.strip_prefix("/messages/") else {
            exact.insert(key.clone(), units.clone());
            continue;
        };
        let Some((mi, path)) = path.split_once("/content/") else {
            continue;
        };
        let Some((bi, field)) = path.split_once('/') else {
            continue;
        };
        let Some((mi, bi)) = mi.parse::<usize>().ok().zip(bi.parse::<usize>().ok()) else {
            continue;
        };
        if let Some((start, last)) = position_map.get(&(mi, bi)) {
            if let Some(projected_units) = text_results.get(&(mi, bi)) {
                if field == "content" {
                    exact.insert(
                        format!("/messages/{mi}/content/{last}/text"),
                        projected_units.clone(),
                    );
                }
            } else {
                exact.insert(
                    format!("/messages/{mi}/content/{start}/{field}"),
                    units.clone(),
                );
            }
        }
    }
    projected.execution.message_json_string_overrides = exact;
    let sources = &request.execution.request_content_source_ids;
    if sources.len() == request.input.messages.len() {
        let mut mapped = Vec::new();
        for (mi, message) in request.input.messages.iter().enumerate() {
            let mut row = Vec::new();
            for bi in 0..message.content.len() {
                let (first, last) = position_map[&(mi, bi)];
                let source = sources[mi].get(bi).cloned().unwrap_or_default();
                row.extend(std::iter::repeat_n(source, last - first + 1));
            }
            mapped.push(row);
        }
        projected.execution.request_content_source_ids = mapped;
    }
    Ok(Cow::Owned(projected))
}

#[cfg(test)]
#[path = "local_tool_search_tests.rs"]
mod tests;
