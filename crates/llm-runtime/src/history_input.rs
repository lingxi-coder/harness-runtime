//! Durable history input boundary. Legacy transcript companions are consumed
//! here once; outbound requests contain only the SDK's canonical model input.
use crate::*;
use base64::Engine as _;
use lingxi_llm_client::protocol as wire;
pub(crate) use lingxi_llm_client::replay::native_family;
use lingxi_llm_client::replay::{
    has_replay_metadata, native_cited_text, ReplayContext, ReplayPolicy,
};
use serde_json::{json, Value};
fn invalid(error: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest {
        message: error.to_string(),
    }
}
fn cache(cache: &CacheControl) -> wire::CacheControl {
    match cache {
        CacheControl::Ephemeral => wire::CacheControl::default(),
        CacheControl::EphemeralScoped { scope, ttl_1h } => wire::CacheControl {
            scope: scope.map(|_| wire::CacheScope::Global),
            ttl: ttl_1h.then(|| serde_json::from_value(json!("1h")).expect("legacy cache TTL")),
        },
    }
}

fn block(block: &ContentBlock) -> Result<wire::ContentBlock, LlmError> {
    let native = |value| wire::ContentBlock::ProviderContent {
        protocol: wire::ProtocolFamily::AnthropicMessages,
        value,
    };
    let base64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(match block {
        ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_native_content" => {
            let block = serde_json::from_value(
                value
                    .get("block")
                    .ok_or_else(|| invalid("native history carrier is missing its SDK block"))?
                    .clone(),
            )
            .map_err(invalid)?;
            if !matches!(block, wire::ContentBlock::Native { .. }) {
                return Err(invalid(
                    "native history carrier must contain an SDK native block",
                ));
            }
            block
        }
        ContentBlock::ProviderContent { protocol, value } => wire::ContentBlock::ProviderContent {
            protocol: serde_json::from_value(json!(protocol)).map_err(invalid)?,
            value: value.clone(),
        },
        ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
            wire::ContentBlock::Text {
                text: text.clone(),
                thought_signature: None,
            }
        }
        ContentBlock::Image { media_type, bytes } => wire::ContentBlock::Image {
            source: wire::ImageSource::Base64 {
                media_type: media_type.clone(),
                data: base64(bytes),
            },
        },
        ContentBlock::ImageUrl { url } => wire::ContentBlock::Image {
            source: wire::ImageSource::Url { url: url.clone() },
        },
        ContentBlock::Document { media_type, bytes } => wire::ContentBlock::Document {
            source: wire::DocumentSource::Base64 {
                media_type: media_type.clone(),
                data: base64(bytes),
            },
            title: None,
        },
        ContentBlock::ToolCall { id, name, input } => wire::ContentBlock::ToolUse {
            id: wire::ToolUseId::new(id),
            name: name.clone(),
            input: input.clone(),
            provider_id: None,
            caller: None,
            toolset_name: None,
            thought_signature: None,
        },
        ContentBlock::ToolResult {
            tool_call_id,
            output,
            is_error,
            cache_control,
            cache_reference,
        } => {
            let exact_text =
                ::lingxi_core::types::js_utf16::tool_result_display(output).map(Value::String);
            let output = exact_text.as_ref().unwrap_or(output);
            if cache_reference.is_some() {
                let content = if output.is_string() || output.is_array() {
                    output.clone()
                } else {
                    Value::String(output.to_string())
                };
                let mut value = json!({"type":"tool_result","tool_use_id":tool_call_id,"content":content,"is_error":is_error});
                if let Some(control) = cache_control {
                    value["cache_control"] = cache(control).wire_value();
                }
                if let Some(reference) = cache_reference {
                    value["cache_reference"] = json!(reference);
                }
                native(value)
            } else {
                wire::ContentBlock::ToolResult {
                    tool_use_id: wire::ToolUseId::new(tool_call_id),
                    content: output
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| output.to_string()),
                    is_error: *is_error,
                    blocks: output.as_array().cloned(),
                    toolset_name: None,
                }
            }
        }
        ContentBlock::Reasoning { text, signature } => wire::ContentBlock::Thinking {
            text: text.clone(),
            signature: signature.clone(),
        },
        ContentBlock::RedactedThinking { data } => {
            wire::ContentBlock::RedactedThinking { data: data.clone() }
        }
        ContentBlock::ServerToolUse { id, name, input } => {
            native(json!({"type":"server_tool_use","id":id,"name":name,"input":input}))
        }
        ContentBlock::ConnectorText {
            connector_text,
            signature,
        } => native(
            json!({"type":"connector_text","connector_text":connector_text,"signature":signature}),
        ),
        ContentBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => native(
            json!({"type":"advisor_tool_result","tool_use_id":tool_use_id,"content":content,"is_error":is_error}),
        ),
        ContentBlock::CacheEdits { edits } => native(json!({"type":"cache_edits","edits":edits})),
    })
}

// Canonical SDK metadata is kept beside the host block in the transcript.
// Rehydrate it before encoding; the companion is never sent as a second part.
pub(crate) fn replay_companion(
    block: &ContentBlock,
) -> Option<(wire::ProtocolFamily, wire::ContentBlock)> {
    let ContentBlock::ProviderContent { protocol, value } = block else {
        return None;
    };
    let family = serde_json::from_value(json!(protocol)).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("lingxi_replay_metadata") {
        return None;
    }
    let block: wire::ContentBlock = serde_json::from_value(value.get("block")?.clone()).ok()?;
    has_replay_metadata(&block).then_some((family, block))
}
/// Match durable companion rows to their visible history block, then ask the SDK
/// to apply the host's explicit cross-provider replay policy. The original block
/// index is retained for exact-string and cache sidecars.
fn project_message(
    message: &Message,
    family: wire::ProtocolFamily,
    computer_ids: &BTreeSet<String>,
    acknowledged_receipts: &ComputerMarkerBoundaries,
    abandoned_calls: &BTreeSet<(String, String)>,
    message_index: usize,
    normalize: bool,
) -> Result<Vec<(usize, wire::ContentBlock)>, LlmError> {
    let mut metadata: Vec<_> = message
        .content
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            replay_companion(block).map(|(source, block)| (index, source, block))
        })
        .collect();
    let mut mapped = Vec::new();
    for (index, item) in message.content.iter().enumerate() {
        if let Some((kind, value, source)) = computer_marker(item) {
            // Independent forks consume the ordinary tool transcript. Native
            // companions carry the main request's receipt/route authority.
            if crate::computer::uses_auxiliary_history_projection() {
                continue;
            }
            let source: wire::ProtocolFamily =
                serde_json::from_value(json!(source)).map_err(invalid)?;
            if native_family(source) != native_family(family) {
                return Err(invalid(
                    "native computer history cannot be sent on another protocol",
                ));
            }
            if kind == "lingxi_computer_continuation" {
                let continuation: wire::ContinuationRef = serde_json::from_value(
                    value
                        .get("continuation")
                        .ok_or_else(|| {
                            invalid("computer continuation marker is missing its reference")
                        })?
                        .clone(),
                )
                .map_err(invalid)?;
                if native_family(continuation.protocol) != native_family(family) {
                    return Err(invalid(
                        "computer continuation marker does not match its protocol",
                    ));
                }
                continue;
            }
            if kind == "lingxi_computer_receipt_ack" {
                continue;
            }
            if kind == "lingxi_computer_abandoned" {
                continue;
            }
            if kind == "lingxi_computer_binding" {
                let call: wire::computer::NativeComputerCall = serde_json::from_value(
                    value
                        .get("call")
                        .ok_or_else(|| invalid("computer binding is missing the original call"))?
                        .clone(),
                )
                .map_err(invalid)?;
                let response_id = value["provider_response_id"]
                    .as_str()
                    .ok_or_else(|| invalid("computer binding is missing its response identity"))?;
                if abandoned_calls.contains(&(response_id.into(), call.context.call_id.clone())) {
                    continue;
                }
            }
            if kind == "lingxi_computer_receipt" {
                let call_id = value
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| {
                        invalid("computer receipt marker is missing its original call ID")
                    })?;
                let continuation_protocol = matches!(
                    native_family(family),
                    wire::ProtocolFamily::OpenAiResponses
                        | wire::ProtocolFamily::GeminiInteractions
                );
                let response_id = value["provider_response_id"]
                    .as_str()
                    .ok_or_else(|| invalid("computer receipt is missing its response identity"))?;
                if abandoned_calls.contains(&(response_id.into(), call_id.into()))
                    || continuation_protocol
                        && marked_after(acknowledged_receipts, call_id, message_index)
                {
                    continue;
                }
            }
            let originals: Vec<wire::ContentBlock> = if kind == "lingxi_computer_binding" {
                serde_json::from_value(
                    value
                        .get("original_blocks")
                        .ok_or_else(|| invalid("computer binding is missing original_blocks"))?
                        .clone(),
                )
                .map_err(invalid)?
            } else {
                vec![serde_json::from_value(
                    value
                        .get("block")
                        .ok_or_else(|| invalid("computer receipt is missing its SDK block"))?
                        .clone(),
                )
                .map_err(invalid)?]
            };
            if originals.is_empty() {
                return Err(invalid("computer binding has no original provider blocks"));
            }
            for original in originals {
                if matches!(&original, wire::ContentBlock::ProviderContent { value, .. }
                    if matches!(value["type"].as_str(), Some("lingxi_computer_binding" | "lingxi_computer_receipt" | "lingxi_computer_continuation" | "lingxi_computer_receipt_ack" | "lingxi_computer_abandoned" | "lingxi_native_content")))
                {
                    return Err(invalid(
                        "computer marker cannot contain another host marker",
                    ));
                }
                mapped.push((index, Some(source), original));
            }
            continue;
        }
        if matches!(item, ContentBlock::ToolCall { id, .. } if computer_ids.contains(id))
            || matches!(item, ContentBlock::ToolResult { tool_call_id, .. } if computer_ids.contains(tool_call_id))
        {
            continue;
        }
        if replay_companion(item).is_some()
            || matches!(item, ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_observation")
        {
            continue;
        }
        let position = metadata
            .iter()
            .position(|(native_index, _, native)| match (item, native) {
                (
                    ContentBlock::ToolCall { id, .. },
                    wire::ContentBlock::ToolUse { id: native_id, .. },
                ) => id == native_id.as_str(),
                (
                    ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. },
                    wire::ContentBlock::Text {
                        text: native_text, ..
                    },
                ) => text == native_text,
                (
                    ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. },
                    native,
                ) if *native_index == index + 1 => native_cited_text(native) == Some(text.as_str()),
                _ => false,
            });
        let (source, mapped_block) = if let Some(position) = position {
            let (_, source, mut native) = metadata.remove(position);
            if let (
                ContentBlock::ToolCall { name, input, .. },
                wire::ContentBlock::ToolUse {
                    name: native_name,
                    input: native_input,
                    ..
                },
            ) = (item, &mut native)
            {
                native_name.clone_from(name);
                native_input.clone_from(input);
            }
            (Some(source), native)
        } else {
            (None, block(item)?)
        };
        mapped.push((index, source, mapped_block));
    }
    let blocks: Vec<_> = mapped.iter().map(|(_, _, block)| block.clone()).collect();
    if !normalize {
        return Ok(mapped
            .into_iter()
            .map(|(index, _, block)| (index, block))
            .collect());
    }
    let context = ReplayContext::for_message(&blocks, family);
    mapped
        .into_iter()
        .filter_map(|(index, source, block)| {
            match context.normalize(&block, source, ReplayPolicy::DropIncompatible) {
                Ok(Some(block)) => Some(Ok((index, block))),
                Ok(None) => None,
                Err(error) => Some(Err(crate::upstream::error(error))),
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn message_content(
    message: &Message,
    family: wire::ProtocolFamily,
) -> Result<Vec<wire::ContentBlock>, LlmError> {
    let ids = computer_tool_ids(std::slice::from_ref(message))?;
    let acknowledged = acknowledged_computer_receipts(std::slice::from_ref(message))?;
    let abandoned = abandoned_computer_calls(std::slice::from_ref(message))?;
    Ok(
        project_message(message, family, &ids, &acknowledged, &abandoned, 0, true)?
            .into_iter()
            .map(|(_, block)| block)
            .collect(),
    )
}

pub fn history_input(
    model: &str,
    messages: &[Message],
    system: &[SystemBlock],
    tools: &[ToolDeclaration],
    protocol: wire::ProtocolFamily,
) -> Result<(wire::ChatRequest, BTreeMap<String, Vec<u16>>), LlmError> {
    let mut result = wire::ChatRequest::new(model);
    let mut overrides = BTreeMap::new();
    let computer_ids = computer_tool_ids(messages)?;
    let acknowledged_receipts = acknowledged_computer_receipts(messages)?;
    let abandoned_calls = abandoned_computer_calls(messages)?;
    let continuation_boundary = computer_continuation_boundary(messages, protocol)?;
    for (message_index, message) in messages.iter().enumerate() {
        if continuation_boundary.is_some_and(|boundary| message_index < boundary) {
            continue;
        }
        let mut projected = project_message(
            message,
            protocol,
            &computer_ids,
            &acknowledged_receipts,
            &abandoned_calls,
            message_index,
            true,
        )?;
        if continuation_boundary == Some(message_index) {
            // The provider already stores this turn. Gemini needs ordinary
            // call metadata to name new receipts; its codec excludes assistant
            // rows. Responses receipts already carry their provider call IDs.
            projected.retain(|(_, block)| {
                protocol == wire::ProtocolFamily::GeminiInteractions
                    && matches!(block, wire::ContentBlock::ToolUse { .. })
            });
        }
        if projected.is_empty() {
            continue;
        }
        if native_family(protocol) == wire::ProtocolFamily::AnthropicMessages {
            for (position, (original, _)) in projected.iter().enumerate() {
                let item = &message.content[*original];
                let control = match item {
                    ContentBlock::Text { cache_control, .. }
                    | ContentBlock::TextJsUtf16 { cache_control, .. }
                    | ContentBlock::ToolResult { cache_control, .. } => cache_control.as_ref(),
                    _ => None,
                };
                if let Some(control) = control {
                    result.prompt_cache.breakpoints.push(wire::CacheBreakpoint {
                        position: wire::CachePosition::Message {
                            index: result.messages.len(),
                            block: position,
                        },
                        scope: cache(control).scope,
                        ttl: match control {
                            CacheControl::EphemeralScoped { ttl_1h: true, .. } => {
                                wire::CacheTtl::OneHour
                            }
                            _ => wire::CacheTtl::FiveMinutes,
                        },
                    });
                }
                let exact = match item {
                    ContentBlock::TextJsUtf16 {
                        utf16_code_units, ..
                    } => Some(("text", utf16_code_units.clone())),
                    ContentBlock::ToolResult { output, .. } => {
                        ::lingxi_core::types::js_utf16::tool_result_units(output)
                            .map(|units| ("content", units))
                    }
                    _ => None,
                };
                if let Some((field, units)) = exact {
                    overrides.insert(
                        format!(
                            "/messages/{}/content/{position}/{field}",
                            result.messages.len()
                        ),
                        units,
                    );
                }
            }
        }
        result.messages.push(wire::ConversationMessage {
            role: serde_json::from_value(json!(message.role)).map_err(invalid)?,
            content: projected.into_iter().map(|(_, block)| block).collect(),
            native_options: Vec::new(),
        });
    }
    let toolsets: BTreeMap<_, _> = result
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            wire::ContentBlock::ToolUse {
                id,
                toolset_name: Some(toolset),
                ..
            } => Some((id.clone(), toolset.clone())),
            _ => None,
        })
        .collect();
    for message in &mut result.messages {
        for block in &mut message.content {
            match block {
                wire::ContentBlock::ToolResult {
                    tool_use_id,
                    toolset_name,
                    ..
                } => {
                    *toolset_name = toolsets.get(tool_use_id).cloned();
                }
                wire::ContentBlock::ProviderContent {
                    protocol: wire::ProtocolFamily::AnthropicMessages,
                    value,
                } if value["type"] == "tool_result" => {
                    if let Some(name) = value["tool_use_id"]
                        .as_str()
                        .and_then(|id| toolsets.get(&wire::ToolUseId::new(id)))
                    {
                        value["toolset_name"] = json!(name);
                    }
                }
                _ => {}
            }
        }
    }
    result.system = system
        .iter()
        .map(|b| wire::SystemBlock {
            text: b.text.clone(),
        })
        .collect();
    // Legacy system markers now use the SDK's positional cache policy.
    if native_family(protocol) == wire::ProtocolFamily::AnthropicMessages {
        for (index, block) in system.iter().enumerate() {
            if let Some(control) = &block.cache_control {
                let ttl = match control {
                    CacheControl::EphemeralScoped { ttl_1h: true, .. } => wire::CacheTtl::OneHour,
                    _ => wire::CacheTtl::FiveMinutes,
                };
                let position = wire::CachePosition::System { index };
                if let Some(existing) = result
                    .prompt_cache
                    .breakpoints
                    .iter_mut()
                    .find(|b| b.position == position)
                {
                    existing.scope = cache(control).scope.or(existing.scope);
                    if existing.ttl != ttl {
                        return Err(invalid("conflicting legacy and typed system cache TTL"));
                    }
                } else {
                    result.prompt_cache.breakpoints.push(wire::CacheBreakpoint {
                        scope: cache(control).scope,
                        position,
                        ttl,
                    });
                }
            }
        }
    }
    result.tools = tools
        .iter()
        .map(|t| wire::ToolSpec {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            strict: t.strict,
            tool_type: t.tool_type.clone(),
            defer_loading: t.defer_loading,
            native_options: Vec::new(),
            extra: Value::Object(t.extra.clone()),
        })
        .collect();
    for tool in &mut result.tools {
        if let Some(extra) = tool.extra.as_object_mut() {
            if let Some(callers) = extra.remove("allowed_callers") {
                tool.set_anthropic_allowed_callers(
                    serde_json::from_value(callers).map_err(invalid)?,
                );
            }
        }
        if tool.strict {
            match lingxi_llm_client::providers::anthropic::strict_schema::to_strict_schema(
                &tool.input_schema,
            ) {
                Ok(schema) => tool.input_schema = schema,
                Err(_) => tool.strict = false,
            }
        }
    }
    Ok((result, overrides))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_edge_consumes_companion_once_and_preserves_tool_metadata() {
        let native = wire::ContentBlock::ToolUse {
            id: wire::ToolUseId::new("call-1"),
            name: "old".into(),
            input: json!({}),
            provider_id: Some("provider-1".into()),
            caller: None,
            toolset_name: Some("browser".into()),
            thought_signature: None,
        };
        let history = vec![Message {
            role: "assistant".into(),
            content: vec![
                ContentBlock::ToolCall {
                    id: "call-1".into(),
                    name: "updated".into(),
                    input: json!({"url":"example"}),
                },
                ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: json!({"type":"lingxi_replay_metadata","block":native}),
                },
            ],
        }];
        let (input, overrides) = history_input(
            "model",
            &history,
            &[],
            &[],
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert!(overrides.is_empty());
        assert_eq!(input.messages[0].content.len(), 1);
        let wire::ContentBlock::ToolUse {
            name,
            input,
            provider_id,
            toolset_name,
            ..
        } = &input.messages[0].content[0]
        else {
            panic!("canonical tool use")
        };
        assert_eq!(name, "updated");
        assert_eq!(input, &json!({"url":"example"}));
        assert_eq!(provider_id.as_deref(), Some("provider-1"));
        assert_eq!(toolset_name.as_deref(), Some("browser"));
    }

    #[test]
    fn filtered_history_remaps_exact_strings_and_typed_cache_positions_together() {
        let history = vec![Message {
            role: "user".into(),
            content: vec![
                ContentBlock::ProviderContent {
                    protocol: "open_ai_responses".into(),
                    value: json!({"type":"reasoning","id":"r"}),
                },
                ContentBlock::TextJsUtf16 {
                    text: "�".into(),
                    utf16_code_units: vec![0xd800],
                    cache_control: Some(CacheControl::Ephemeral),
                },
            ],
        }];
        let (input, overrides) = history_input(
            "model",
            &history,
            &[],
            &[],
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(input.messages[0].content.len(), 1);
        assert!(matches!(
            input.messages[0].content[0],
            wire::ContentBlock::Text { .. }
        ));
        assert_eq!(overrides["/messages/0/content/0/text"], vec![0xd800]);
        assert_eq!(
            input.prompt_cache.breakpoints[0].position,
            wire::CachePosition::Message { index: 0, block: 0 }
        );
    }

    fn encoded_body(request: &wire::ChatRequest, protocol: wire::ProtocolFamily) -> Value {
        use lingxi_llm_client::{CodecContext, EncodeRequest, RequestMode, WireCodec};
        let (provider, endpoint) = match protocol {
            wire::ProtocolFamily::AnthropicMessages => ("anthropic", "https://api.anthropic.com"),
            wire::ProtocolFamily::OpenAiResponses => ("openai", "https://api.openai.com/v1"),
            wire::ProtocolFamily::GeminiInteractions => {
                ("gemini", "https://generativelanguage.googleapis.com/v1beta")
            }
            _ => unreachable!(),
        };
        let profile: wire::ProviderProfile = serde_json::from_value(json!({
            "provider_id":provider,"profile_name":provider,"base_url":endpoint,"protocol":protocol,
            "auth":"none","models":[],"extra":{"supports_previous_response_id":true}
        }))
        .unwrap();
        let context = CodecContext::new(&profile, &request.model, RequestMode::Complete)
            .with_account_scope(Some(
                request
                    .continuation
                    .as_ref()
                    .map_or("fixture-account", |reference| {
                        reference.account_scope.as_str()
                    }),
            ));
        let codec: &dyn WireCodec = match protocol {
            wire::ProtocolFamily::AnthropicMessages => &lingxi_llm_client::AnthropicMessagesCodec,
            wire::ProtocolFamily::OpenAiResponses => &lingxi_llm_client::OpenAiResponsesCodec,
            wire::ProtocolFamily::GeminiInteractions => &lingxi_llm_client::GeminiInteractionsCodec,
            _ => unreachable!(),
        };
        let encoded = codec
            .encode_request(EncodeRequest::new(request), &context)
            .unwrap();
        serde_json::from_slice(&encoded.body).unwrap()
    }

    fn management_function(request: &mut wire::ChatRequest) {
        request.tools.push(serde_json::from_value(json!({"name":"computer","description":"Manage computer access",
            "input_schema":{"type":"object","properties":{"action":{"type":"string"}},"required":["action"]}})).unwrap());
    }

    fn computer_binding(ids: &[&str]) -> ContentBlock {
        let original: wire::ContentBlock = serde_json::from_value(json!({
            "type":"tool_use", "id":"native-call", "name":"screenshot", "input":{}, "toolset_name":"computer"
        })).unwrap();
        ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: json!({"type":"lingxi_computer_binding", "provider_response_id":"old-response", "tool_use_ids":ids,
                "call":{"context":{"provider":"anthropic","protocol_version":"computer_toolset_20260801",
                    "call_id":"native-call","item_id":null,"member_name":"screenshot","call_index":0,
                    "action_count":1,"pending_safety_checks":[],"continuation":null,"opaque":{}},
                    "operations":[{"type":"screenshot"}],"requires_screenshot":false},
                "original_blocks":[original]}),
        }
    }

    fn native_round(
        protocol: wire::ProtocolFamily,
        response_id: &str,
        observation: &str,
    ) -> (Message, Message, Option<wire::ContinuationRef>) {
        let (provider, provider_id, endpoint, model, original) = match protocol {
            wire::ProtocolFamily::AnthropicMessages => (
                wire::NativeComputerProvider::Anthropic, "anthropic", "https://api.anthropic.com", "claude-opus-4-8",
                serde_json::from_value(json!({"type":"tool_use","id":"reused-call","name":"screenshot","input":{},"toolset_name":"computer"})).unwrap(),
            ),
            wire::ProtocolFamily::OpenAiResponses => (
                wire::NativeComputerProvider::OpenAi, "openai", "https://api.openai.com/v1", "gpt-5.5",
                lingxi_llm_client::providers::openai::computer::OpenAiComputerCall::from_response_item(&json!({
                    "type":"computer_call","id":format!("item-{response_id}"),"call_id":"reused-call","status":"completed",
                    "actions":[{"type":"screenshot"}],"pending_safety_checks":[]
                })).unwrap().into_content_block().unwrap(),
            ),
            wire::ProtocolFamily::GeminiInteractions => (
                wire::NativeComputerProvider::Gemini, "gemini", "https://generativelanguage.googleapis.com/v1beta", "gemini-3.8-flash",
                wire::ContentBlock::Native {value:wire::NativeExtension::new(
                    lingxi_llm_client::providers::google::computer::CALL_FORMAT,
                    json!({"type":"function_call","id":"reused-call","name":"take_screenshot","arguments":{}})
                ).unwrap()},
            ),
            _ => unreachable!(),
        };
        let continuation = (protocol != wire::ProtocolFamily::AnthropicMessages).then(|| {
            serde_json::from_value::<wire::ContinuationRef>(json!({
                "protocol":protocol,"response_id":response_id,"provider_id":provider_id,"profile_name":provider_id,
                "endpoint_fingerprint":lingxi_llm_client::files::provider_file_endpoint_fingerprint(endpoint),
                "account_scope":"fixture-account","request_model":model
            })).unwrap()
        });
        let call = wire::decode_computer_calls(
            provider,
            std::slice::from_ref(&original),
            continuation.as_ref(),
            &wire::ComputerFrame {
                width: 800,
                height: 600,
                geometry_version: "screen".into(),
            },
        )
        .unwrap()
        .remove(0);
        let image = base64::engine::general_purpose::STANDARD.encode(observation);
        let receipt = wire::encode_computer_receipt(&call, &wire::ComputerReceiptInput {
            results: vec![wire::NativeComputerResult {
                operation_index:0, status:wire::NativeExecutionStatus::Succeeded, content:observation.into(),
                blocks:Some(vec![json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":image}})]),
            }],
            acknowledged_safety_checks:vec![],
        }).unwrap();
        let source = serde_json::to_value(protocol)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        let synthetic = format!("synthetic-{response_id}");
        let mut content = vec![
            ContentBlock::ToolCall {
                id: synthetic.clone(),
                name: "computer".into(),
                input: json!({"action":"screenshot"}),
            },
            ContentBlock::ProviderContent {
                protocol: source.clone(),
                value: json!({
                    "type":"lingxi_computer_binding","provider_response_id":response_id,"call":call,"tool_use_ids":[synthetic],"original_blocks":[original]
                }),
            },
        ];
        if let Some(reference) = &continuation {
            content.push(ContentBlock::ProviderContent {
                protocol: source.clone(),
                value: json!({
                    "type":"lingxi_computer_continuation","continuation":reference
                }),
            });
        }
        (
            Message {
                role: "assistant".into(),
                content,
            },
            Message {
                role: "user".into(),
                content: vec![
                    ContentBlock::ToolResult {
                        tool_call_id: synthetic,
                        output: json!(observation),
                        is_error: None,
                        cache_control: None,
                        cache_reference: None,
                    },
                    ContentBlock::ProviderContent {
                        protocol: source,
                        value: json!({
                            "type":"lingxi_computer_receipt","provider_response_id":response_id,"call_id":"reused-call","block":receipt
                        }),
                    },
                ],
            },
            continuation,
        )
    }

    #[tokio::test]
    async fn independent_computer_history_keeps_ordinary_results_across_provider_routes() {
        for source in [
            wire::ProtocolFamily::GeminiInteractions,
            wire::ProtocolFamily::OpenAiResponses,
            wire::ProtocolFamily::AnthropicMessages,
        ] {
            let (call, receipt, continuation) =
                native_round(source, "pending-response", "actual desktop observation");
            let messages = vec![call, receipt];
            for destination in [
                wire::ProtocolFamily::GeminiInteractions,
                wire::ProtocolFamily::OpenAiResponses,
                wire::ProtocolFamily::AnthropicMessages,
            ] {
                let model = match destination {
                    wire::ProtocolFamily::AnthropicMessages => "claude-opus-4-8",
                    wire::ProtocolFamily::OpenAiResponses => "gpt-5.5",
                    wire::ProtocolFamily::GeminiInteractions => "gemini-3.8-flash",
                    _ => unreachable!(),
                };
                crate::computer::scope_computer_request(
                    Some(crate::computer::ComputerRequestProjection {
                        native: None,
                        continuation: continuation.clone(),
                        binding: None,
                        submission: None,
                    }),
                    async {
                        let (request, _) = crate::computer::without_computer_request(|| {
                            history_input(model, &messages, &[], &[], destination)
                        })
                        .unwrap();
                        assert!(request.continuation.is_none());
                        let body = encoded_body(&request, destination);
                        let body = body.to_string();
                        assert!(
                            body.contains("synthetic-pending-response"),
                            "{source:?} -> {destination:?}: {body}"
                        );
                        assert!(body.contains("actual desktop observation"));
                        for excluded in [
                            "reused-call",
                            "toolset_name",
                            "_sdk_continuation",
                            "previous_response_id",
                            "previous_interaction_id",
                            "lingxi_computer",
                        ] {
                            assert!(
                                !body.contains(excluded),
                                "auxiliary wire contains native authority {excluded}: {body}"
                            );
                        }
                    },
                )
                .await;
                // Main replay still rejects a foreign native route.
                if source != destination {
                    assert!(history_input(model, &messages, &[], &[], destination).is_err());
                }
            }
        }
    }

    fn assert_current_receipt(body: &Value, protocol: wire::ProtocolFamily) {
        let current_image = base64::engine::general_purpose::STANDARD.encode("current observation");
        let obsolete_image =
            base64::engine::general_purpose::STANDARD.encode("obsolete observation");
        assert!(
            body.to_string().contains(&current_image),
            "{protocol:?}: {body}"
        );
        assert!(
            !body.to_string().contains(&obsolete_image),
            "{protocol:?}: {body}"
        );
        assert!(!body.to_string().contains("synthetic-"));
        assert!(!body.to_string().contains("lingxi_computer"));
        match protocol {
            wire::ProtocolFamily::AnthropicMessages => {
                let messages = body["messages"].as_array().unwrap();
                let blocks: Vec<_> = messages
                    .iter()
                    .flat_map(|message| message["content"].as_array().unwrap())
                    .collect();
                let calls: Vec<_> = blocks
                    .iter()
                    .filter(|block| block["type"] == "tool_use")
                    .collect();
                let results: Vec<_> = blocks
                    .iter()
                    .filter(|block| block["type"] == "tool_result")
                    .collect();
                assert_eq!(calls.len(), 1);
                assert_eq!(results.len(), 1);
                assert_eq!(calls[0]["id"], "reused-call");
                assert_eq!(results[0]["tool_use_id"], "reused-call");
                assert_eq!(calls[0]["toolset_name"], "computer");
                assert_eq!(results[0]["toolset_name"], "computer");
            }
            wire::ProtocolFamily::OpenAiResponses => {
                assert_eq!(body["previous_response_id"], "current-response");
                assert_eq!(body["input"].as_array().unwrap().len(), 1);
                assert_eq!(body["input"][0]["type"], "computer_call_output");
                assert_eq!(body["input"][0]["call_id"], "reused-call");
            }
            wire::ProtocolFamily::GeminiInteractions => {
                assert_eq!(body["previous_interaction_id"], "current-response");
                assert_eq!(body["input"].as_array().unwrap().len(), 1);
                assert_eq!(body["input"][0]["type"], "function_result");
                assert_eq!(body["input"][0]["call_id"], "reused-call");
                assert!(body["input"][0].get("_sdk_continuation").is_none());
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn old_ack_does_not_suppress_reused_call_id_on_new_response() {
        for protocol in [
            wire::ProtocolFamily::OpenAiResponses,
            wire::ProtocolFamily::GeminiInteractions,
        ] {
            let (old_call, old_receipt, _) =
                native_round(protocol, "old-response", "obsolete observation");
            let (mut current_call, current_receipt, continuation) =
                native_round(protocol, "current-response", "current observation");
            let source = serde_json::to_value(protocol)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            // The response accepting the old receipt can contain a new call with
            // the same opaque ID. Its ACK applies only to earlier messages.
            current_call.content.push(ContentBlock::ProviderContent {protocol:source,value:json!({
                "type":"lingxi_computer_receipt_ack","call_ids":["reused-call"],"provider_response_id":"current-response"
            })});
            let messages = [old_call, old_receipt, current_call, current_receipt];
            let model = continuation.as_ref().unwrap().request_model.clone();
            let (mut request, _) = crate::computer::scope_computer_request(
                Some(crate::computer::ComputerRequestProjection {
                    native: None,
                    continuation: continuation.clone(),
                    binding: None,
                    submission: None,
                }),
                async { history_input(&model, &messages, &[], &[], protocol).unwrap() },
            )
            .await;
            request.continuation = continuation;
            management_function(&mut request);
            assert!(request.native_options.is_empty());
            assert_eq!(request.tools[0].name, "computer");
            assert_current_receipt(&encoded_body(&request, protocol), protocol);
        }
    }

    #[tokio::test]
    async fn old_abandon_does_not_remove_reused_call_id_on_new_response() {
        for protocol in [
            wire::ProtocolFamily::AnthropicMessages,
            wire::ProtocolFamily::OpenAiResponses,
            wire::ProtocolFamily::GeminiInteractions,
        ] {
            let (old_call, old_receipt, _) =
                native_round(protocol, "old-response", "obsolete observation");
            let (current_call, current_receipt, continuation) =
                native_round(protocol, "current-response", "current observation");
            let source = serde_json::to_value(protocol)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            let messages = [
                old_call,
                old_receipt,
                Message {
                    role: "user".into(),
                    content: vec![
                        ContentBlock::ProviderContent {
                            protocol: source,
                            value: json!({
                                "type":"lingxi_computer_abandoned","call_ids":["reused-call"],"call_identities":[["old-response","reused-call"]],"reason":"Previous outcome unknown; obtain a fresh observation"
                            }),
                        },
                        ContentBlock::Text {
                            text: "Obtain a fresh observation".into(),
                            citations: None,
                            cache_control: None,
                        },
                    ],
                },
                current_call,
                current_receipt,
            ];
            let model = continuation
                .as_ref()
                .map_or("claude-opus-4-8", |reference| {
                    reference.request_model.as_str()
                });
            let (mut request, _) = crate::computer::scope_computer_request(
                Some(crate::computer::ComputerRequestProjection {
                    native: None,
                    continuation: continuation.clone(),
                    binding: None,
                    submission: None,
                }),
                async { history_input(model, &messages, &[], &[], protocol).unwrap() },
            )
            .await;
            request.continuation = continuation;
            management_function(&mut request);
            let body = encoded_body(&request, protocol);
            assert_current_receipt(&body, protocol);
            if protocol == wire::ProtocolFamily::AnthropicMessages {
                assert!(body.to_string().contains("Obtain a fresh observation"));
            }
        }
    }

    #[test]
    fn late_abandon_marker_does_not_remove_a_new_response_with_reused_call_id() {
        let protocol = wire::ProtocolFamily::AnthropicMessages;
        let (old_call, old_receipt, _) =
            native_round(protocol, "old-response", "obsolete observation");
        let (current_call, mut current_receipt, _) =
            native_round(protocol, "current-response", "current observation");
        current_receipt.content.push(ContentBlock::ProviderContent {
            protocol:"anthropic_messages".into(),
            value:json!({"type":"lingxi_computer_abandoned","call_ids":["reused-call"],"call_identities":[["old-response","reused-call"]],"reason":"Previous outcome unknown; observe again"}),
        });
        let (mut request, _) = history_input(
            "claude-opus-4-8",
            &[old_call, old_receipt, current_call, current_receipt],
            &[],
            &[],
            protocol,
        )
        .unwrap();
        management_function(&mut request);
        assert_current_receipt(&encoded_body(&request, protocol), protocol);
    }

    #[test]
    fn computer_markers_restore_old_binding_and_strip_only_mapped_rows() {
        let synthetic = ContentBlock::ToolCall {
            id: "synthetic".into(),
            name: "computer".into(),
            input: json!({"action":"screenshot"}),
        };
        let unrelated = ContentBlock::ToolCall {
            id: "ordinary".into(),
            name: "computer".into(),
            input: json!({"action":"get_access"}),
        };
        let result = |id: &str| ContentBlock::ToolResult {
            tool_call_id: id.into(),
            output: json!("done"),
            is_error: None,
            cache_control: None,
            cache_reference: None,
        };
        let receipt: wire::ContentBlock = serde_json::from_value(json!({
            "type":"tool_result","tool_use_id":"native-call","content":"final hook output","toolset_name":"computer"
        })).unwrap();
        let messages = vec![
            Message {
                role: "assistant".into(),
                content: vec![synthetic, unrelated, computer_binding(&["synthetic"])],
            },
            Message {
                role: "user".into(),
                content: vec![
                    result("synthetic"),
                    result("ordinary"),
                    ContentBlock::ProviderContent {
                        protocol: "anthropic_messages".into(),
                        value: json!({"type":"lingxi_computer_receipt","provider_response_id":"old-response","call_id":"native-call","block":receipt}),
                    },
                ],
            },
        ];
        let (request, _) = history_input(
            "model",
            &messages,
            &[],
            &[],
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(request.messages[0].content.len(), 2);
        assert!(
            matches!(&request.messages[0].content[0], wire::ContentBlock::ToolUse{id,..} if id.as_str()=="ordinary")
        );
        assert!(
            matches!(&request.messages[0].content[1], wire::ContentBlock::ToolUse{id,toolset_name,..} if id.as_str()=="native-call" && toolset_name.as_deref()==Some("computer"))
        );
        assert_eq!(
            request.messages[1].content,
            vec![block(&result("ordinary")).unwrap(), receipt]
        );
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(!encoded.contains("lingxi_computer"));
        assert!(!encoded.contains("synthetic"));
    }

    #[test]
    fn computer_history_rejects_protocol_switch_and_malformed_binding() {
        let message = Message {
            role: "assistant".into(),
            content: vec![computer_binding(&["synthetic"])],
        };
        assert!(history_input(
            "model",
            &[message],
            &[],
            &[],
            wire::ProtocolFamily::OpenAiResponses
        )
        .is_err());
        let mut malformed = computer_binding(&["same", "same"]);
        assert!(history_input(
            "model",
            &[Message {
                role: "assistant".into(),
                content: vec![malformed.clone()]
            }],
            &[],
            &[],
            wire::ProtocolFamily::AnthropicMessages
        )
        .is_err());
        if let ContentBlock::ProviderContent { value, .. } = &mut malformed {
            value["tool_use_ids"] = json!(["single"]);
            value.as_object_mut().unwrap().remove("original_blocks");
        }
        assert!(history_input(
            "model",
            &[Message {
                role: "assistant".into(),
                content: vec![malformed]
            }],
            &[],
            &[],
            wire::ProtocolFamily::AnthropicMessages
        )
        .is_err());
    }

    #[test]
    fn acknowledged_computer_receipt_is_not_submitted_again_on_continuation_protocols() {
        let receipt: wire::ContentBlock = serde_json::from_value(json!({
            "type":"tool_result","tool_use_id":"native-call","content":"old image","toolset_name":"computer"
        })).unwrap();
        for protocol in [
            wire::ProtocolFamily::OpenAiResponses,
            wire::ProtocolFamily::GeminiInteractions,
        ] {
            let source = serde_json::to_value(protocol)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            let messages = [
                Message {
                    role: "user".into(),
                    content: vec![ContentBlock::ProviderContent {
                        protocol: source.clone(),
                        value: json!({"type":"lingxi_computer_receipt","provider_response_id":"old-response","call_id":"native-call","block":receipt}),
                    }],
                },
                Message {
                    role: "assistant".into(),
                    content: vec![ContentBlock::ProviderContent {
                        protocol: source,
                        value: json!({"type":"lingxi_computer_receipt_ack","call_ids":["native-call"]}),
                    }],
                },
            ];
            let (request, _) = history_input("model", &messages, &[], &[], protocol).unwrap();
            assert!(request.messages.is_empty());
        }
    }

    #[test]
    fn acknowledged_claude_receipt_pairs_with_original_call_on_next_function_request() {
        let receipt: wire::ContentBlock = serde_json::from_value(json!({
            "type":"tool_result","tool_use_id":"native-call","content":"final screenshot","toolset_name":"computer"
        })).unwrap();
        let messages = [
            Message {
                role: "assistant".into(),
                content: vec![computer_binding(&["synthetic"])],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: json!({"type":"lingxi_computer_receipt","provider_response_id":"old-response","call_id":"native-call","block":receipt}),
                }],
            },
            Message {
                role: "assistant".into(),
                content: vec![ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: json!({"type":"lingxi_computer_receipt_ack","call_ids":["native-call"]}),
                }],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "Check access again".into(),
                    citations: None,
                    cache_control: None,
                }],
            },
        ];
        let (mut request, _) = history_input(
            "claude-opus-4-8",
            &messages,
            &[],
            &[],
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        management_function(&mut request);
        let body = encoded_body(&request, wire::ProtocolFamily::AnthropicMessages);
        assert_eq!(body["tools"][0]["name"], "computer");
        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][0]["content"][0]["id"], "native-call");
        assert_eq!(
            body["messages"][0]["content"][0]["toolset_name"],
            "computer"
        );
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
        assert_eq!(
            body["messages"][1]["content"][0]["tool_use_id"],
            "native-call"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["toolset_name"],
            "computer"
        );
        assert!(!body.to_string().contains("lingxi_computer"));
    }

    #[test]
    fn abandoned_native_work_is_omitted_from_fresh_function_history() {
        for protocol in [
            wire::ProtocolFamily::AnthropicMessages,
            wire::ProtocolFamily::OpenAiResponses,
            wire::ProtocolFamily::GeminiInteractions,
        ] {
            let source = serde_json::to_value(protocol)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            let (provider, original) = match protocol {
                wire::ProtocolFamily::AnthropicMessages => (wire::NativeComputerProvider::Anthropic,
                    serde_json::from_value(json!({"type":"tool_use","id":"native-call","name":"screenshot","input":{},"toolset_name":"computer"})).unwrap()),
                wire::ProtocolFamily::OpenAiResponses => (wire::NativeComputerProvider::OpenAi,
                    lingxi_llm_client::providers::openai::computer::OpenAiComputerCall::from_response_item(&json!({
                        "type":"computer_call","id":"native-item","call_id":"native-call","status":"completed",
                        "actions":[{"type":"screenshot"}],"pending_safety_checks":[]
                    })).unwrap().into_content_block().unwrap()),
                _ => (wire::NativeComputerProvider::Gemini, wire::ContentBlock::Native {value:wire::NativeExtension::new(
                    lingxi_llm_client::providers::google::computer::CALL_FORMAT,
                    json!({"type":"function_call","id":"native-call","name":"take_screenshot","arguments":{}})
                ).unwrap()}),
            };
            let continuation:wire::ContinuationRef = serde_json::from_value(json!({
                "protocol":protocol,"response_id":"old-response","provider_id":"provider","profile_name":"profile",
                "endpoint_fingerprint":"endpoint","account_scope":"account","request_model":"model"
            })).unwrap();
            let calls = wire::decode_computer_calls(
                provider,
                std::slice::from_ref(&original),
                Some(&continuation),
                &wire::ComputerFrame {
                    width: 800,
                    height: 600,
                    geometry_version: "screen".into(),
                },
            )
            .unwrap();
            let binding = ContentBlock::ProviderContent {
                protocol: source.clone(),
                value: json!({
                    "type":"lingxi_computer_binding","provider_response_id":"old-response","call":calls[0],"tool_use_ids":["synthetic"],"original_blocks":[original]
                }),
            };
            let receipt:wire::ContentBlock = serde_json::from_value(json!({"type":"tool_result","tool_use_id":"native-call","content":"obsolete receipt","toolset_name":"computer"})).unwrap();
            let messages = [
                Message {
                    role: "assistant".into(),
                    content: vec![
                        ContentBlock::ToolCall {
                            id: "ordinary".into(),
                            name: "computer".into(),
                            input: json!({"action":"get_access"}),
                        },
                        ContentBlock::ToolCall {
                            id: "synthetic".into(),
                            name: "computer".into(),
                            input: json!({"action":"screenshot"}),
                        },
                        binding,
                    ],
                },
                Message {
                    role: "user".into(),
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_call_id: "ordinary".into(),
                            output: json!("normal audit result"),
                            is_error: None,
                            cache_control: None,
                            cache_reference: None,
                        },
                        ContentBlock::ToolResult {
                            tool_call_id: "synthetic".into(),
                            output: json!("old synthetic result"),
                            is_error: None,
                            cache_control: None,
                            cache_reference: None,
                        },
                        ContentBlock::ProviderContent {
                            protocol: source.clone(),
                            value: json!({"type":"lingxi_computer_receipt","provider_response_id":"old-response","call_id":"native-call","block":receipt}),
                        },
                    ],
                },
                Message {
                    role: "user".into(),
                    content: vec![
                        ContentBlock::ProviderContent {
                            protocol: source,
                            value: json!({"type":"lingxi_computer_abandoned","call_ids":["native-call"],"call_identities":[["old-response","native-call"]],"reason":"started action outcome unknown; observe again"}),
                        },
                        ContentBlock::Text {
                            text: "Execution outcome unknown; perform a fresh observation".into(),
                            citations: None,
                            cache_control: None,
                        },
                    ],
                },
            ];
            let model = match protocol {
                wire::ProtocolFamily::AnthropicMessages => "claude-opus-4-8",
                wire::ProtocolFamily::OpenAiResponses => "gpt-5.5",
                _ => "gemini-3.8-flash",
            };
            let (mut request, _) = history_input(model, &messages, &[], &[], protocol).unwrap();
            management_function(&mut request);
            let body = encoded_body(&request, protocol).to_string();
            for missing in [
                "native-call",
                "obsolete receipt",
                "synthetic",
                "lingxi_computer",
            ] {
                assert!(!body.contains(missing), "{protocol:?}: {body}");
            }
            assert!(body.contains("normal audit result"));
            assert!(body.contains("ordinary"));
            assert!(body.contains("perform a fresh observation"));
            assert!(request.continuation.is_none());
        }
    }

    #[test]
    fn malformed_abandoned_markers_fail_closed() {
        for value in [
            json!({"type":"lingxi_computer_abandoned","call_ids":[],"reason":"unknown"}),
            json!({"type":"lingxi_computer_abandoned","call_ids":["id"],"reason":""}),
            json!({"type":"lingxi_computer_abandoned","call_ids":["id","id"],"reason":"unknown"}),
            json!({"type":"lingxi_computer_abandoned","call_ids":["id"],"reason":"unknown","force":true}),
        ] {
            let message = Message {
                role: "user".into(),
                content: vec![ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value,
                }],
            };
            assert!(history_input(
                "model",
                &[message],
                &[],
                &[],
                wire::ProtocolFamily::AnthropicMessages
            )
            .is_err());
        }
    }

    #[tokio::test]
    async fn interactions_continuation_sends_only_new_user_input() {
        let continuation: wire::ContinuationRef = serde_json::from_value(json!({
            "protocol":"gemini_interactions","response_id":"interaction-current","provider_id":"gemini",
            "profile_name":"gemini","endpoint_fingerprint":"endpoint","account_scope":"account","request_model":"model"
        })).unwrap();
        let messages = [
            Message {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "old user input".into(),
                    citations: None,
                    cache_control: None,
                }],
            },
            Message {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::ToolCall {
                        id: "current-call".into(),
                        name: "Read".into(),
                        input: json!({}),
                    },
                    ContentBlock::ProviderContent {
                        protocol: "gemini_interactions".into(),
                        value: json!({"type":"lingxi_computer_continuation","continuation":continuation}),
                    },
                ],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "current-call".into(),
                    output: json!("new tool result"),
                    is_error: None,
                    cache_control: None,
                    cache_reference: None,
                }],
            },
        ];
        let (request, _) = crate::computer::scope_computer_request(
            Some(crate::computer::ComputerRequestProjection {
                native: None,
                continuation: Some(continuation),
                binding: None,
                submission: None,
            }),
            async {
                history_input(
                    "model",
                    &messages,
                    &[],
                    &[],
                    wire::ProtocolFamily::GeminiInteractions,
                )
                .unwrap()
            },
        )
        .await;
        assert_eq!(request.messages.len(), 2);
        assert!(
            matches!(&request.messages[0].content[0], wire::ContentBlock::ToolUse{id,..} if id.as_str()=="current-call")
        );
        assert!(
            matches!(&request.messages[1].content[0], wire::ContentBlock::ToolResult{content,..} if content=="new tool result")
        );
        assert!(!serde_json::to_string(&request)
            .unwrap()
            .contains("old user input"));
    }

    #[tokio::test]
    async fn responses_computer_continuation_omits_historical_function_outputs() {
        let continuation: wire::ContinuationRef = serde_json::from_value(json!({
            "protocol":"open_ai_responses","response_id":"response-current","provider_id":"openai",
            "profile_name":"openai","endpoint_fingerprint":lingxi_llm_client::files::provider_file_endpoint_fingerprint("https://api.openai.com/v1"),
            "account_scope":"account","request_model":"model"
        })).unwrap();
        let result = |id: &str, text: &str| ContentBlock::ToolResult {
            tool_call_id: id.into(),
            output: json!(text),
            is_error: None,
            cache_control: None,
            cache_reference: None,
        };
        let messages = [
            Message {
                role: "assistant".into(),
                content: vec![ContentBlock::ToolCall {
                    id: "old-call".into(),
                    name: "computer".into(),
                    input: json!({"action":"get_access"}),
                }],
            },
            Message {
                role: "user".into(),
                content: vec![result("old-call", "old accepted output")],
            },
            Message {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::ToolCall {
                        id: "current-call".into(),
                        name: "computer".into(),
                        input: json!({"action":"get_access"}),
                    },
                    ContentBlock::ProviderContent {
                        protocol: "open_ai_responses".into(),
                        value: json!({"type":"lingxi_computer_continuation","continuation":continuation}),
                    },
                ],
            },
            Message {
                role: "user".into(),
                content: vec![result("current-call", "new output")],
            },
        ];
        let (mut request, _) = crate::computer::scope_computer_request(
            Some(crate::computer::ComputerRequestProjection {
                native: None,
                continuation: Some(continuation.clone()),
                binding: None,
                submission: None,
            }),
            async {
                history_input(
                    "model",
                    &messages,
                    &[],
                    &[],
                    wire::ProtocolFamily::OpenAiResponses,
                )
                .unwrap()
            },
        )
        .await;
        request.continuation = Some(continuation);
        management_function(&mut request);
        let body = encoded_body(&request, wire::ProtocolFamily::OpenAiResponses);
        assert_eq!(body["previous_response_id"], "response-current");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["type"], "function_call_output");
        assert_eq!(body["input"][0]["call_id"], "current-call");
        assert_eq!(body["input"][0]["output"], "new output");
        assert!(!body.to_string().contains("old-call"));
        assert!(!body.to_string().contains("old accepted output"));
    }
}
use std::collections::{BTreeMap, BTreeSet};

fn computer_marker(block: &ContentBlock) -> Option<(&str, &Value, &str)> {
    let ContentBlock::ProviderContent { protocol, value } = block else {
        return None;
    };
    let kind = value.get("type")?.as_str()?;
    matches!(
        kind,
        "lingxi_computer_binding"
            | "lingxi_computer_receipt"
            | "lingxi_computer_continuation"
            | "lingxi_computer_receipt_ack"
            | "lingxi_computer_abandoned"
    )
    .then_some((kind, value, protocol))
}

fn computer_tool_ids(messages: &[Message]) -> Result<BTreeSet<String>, LlmError> {
    let mut ids = BTreeSet::new();
    if crate::computer::uses_auxiliary_history_projection() {
        return Ok(ids);
    }
    for item in messages.iter().flat_map(|message| &message.content) {
        if let Some(("lingxi_computer_binding", value, _)) = computer_marker(item) {
            let mapped: Vec<String> = serde_json::from_value(
                value
                    .get("tool_use_ids")
                    .ok_or_else(|| invalid("computer binding is missing tool_use_ids"))?
                    .clone(),
            )
            .map_err(invalid)?;
            if mapped.is_empty()
                || mapped
                    .iter()
                    .any(|id| id.is_empty() || !ids.insert(id.clone()))
            {
                return Err(invalid(
                    "computer binding has empty or duplicate synthetic tool IDs",
                ));
            }
            // Validate the saved provider call even though outgoing projection never executes it.
            let _: wire::computer::NativeComputerCall = serde_json::from_value(
                value
                    .get("call")
                    .ok_or_else(|| invalid("computer binding is missing the original call"))?
                    .clone(),
            )
            .map_err(invalid)?;
        }
    }
    Ok(ids)
}
// Provider call IDs are opaque and can recur on a later response. A marker
// only retires work in earlier messages, never a new call in its own response.
// Keeping the latest marker index is sufficient to test that temporal boundary.
type ComputerMarkerBoundaries = BTreeMap<String, usize>;

fn marked_after(markers: &ComputerMarkerBoundaries, call_id: &str, message_index: usize) -> bool {
    markers
        .get(call_id)
        .is_some_and(|index| *index > message_index)
}

fn acknowledged_computer_receipts(
    messages: &[Message],
) -> Result<ComputerMarkerBoundaries, LlmError> {
    let mut ids = BTreeMap::new();
    if crate::computer::uses_auxiliary_history_projection() {
        return Ok(ids);
    }
    for (message_index, block) in messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| message.content.iter().map(move |block| (index, block)))
    {
        if let Some(("lingxi_computer_receipt_ack", value, _)) = computer_marker(block) {
            let calls: Vec<String> = serde_json::from_value(
                value
                    .get("call_ids")
                    .ok_or_else(|| invalid("computer receipt acknowledgment is missing call_ids"))?
                    .clone(),
            )
            .map_err(invalid)?;
            if calls.is_empty() || calls.iter().any(|id| id.trim().is_empty()) {
                return Err(invalid(
                    "computer receipt acknowledgment requires nonempty call IDs",
                ));
            }
            ids.extend(calls.into_iter().map(|id| (id, message_index)));
        }
    }
    Ok(ids)
}

fn abandoned_computer_calls(messages: &[Message]) -> Result<BTreeSet<(String, String)>, LlmError> {
    let mut ids = BTreeSet::new();
    if crate::computer::uses_auxiliary_history_projection() {
        return Ok(ids);
    }
    for block in messages.iter().flat_map(|message| &message.content) {
        if let Some(("lingxi_computer_abandoned", value, _)) = computer_marker(block) {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("computer abandoned marker must be an object"))?;
            if object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "call_ids" | "call_identities" | "reason"
                )
            }) {
                return Err(invalid(
                    "computer abandoned marker contains unsupported fields",
                ));
            }
            if value
                .get("reason")
                .and_then(Value::as_str)
                .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(invalid(
                    "computer abandoned marker requires a nonempty recovery reason",
                ));
            }
            let calls: Vec<String> = serde_json::from_value(
                value
                    .get("call_ids")
                    .ok_or_else(|| invalid("computer abandoned marker is missing call_ids"))?
                    .clone(),
            )
            .map_err(invalid)?;
            let mut unique = BTreeSet::new();
            if calls.is_empty()
                || calls
                    .iter()
                    .any(|id| id.trim().is_empty() || !unique.insert(id.clone()))
            {
                return Err(invalid(
                    "computer abandoned marker requires distinct nonempty call IDs",
                ));
            }
            let identities: Vec<(String, String)> = serde_json::from_value(
                value
                    .get("call_identities")
                    .ok_or_else(|| {
                        invalid("computer abandoned marker is missing scoped call identities")
                    })?
                    .clone(),
            )
            .map_err(invalid)?;
            let scoped: BTreeSet<_> = identities.iter().cloned().collect();
            let scoped_calls: BTreeSet<_> =
                identities.iter().map(|(_, call)| call.clone()).collect();
            if identities.is_empty()
                || scoped.len() != identities.len()
                || identities
                    .iter()
                    .any(|(response, call)| response.trim().is_empty() || call.trim().is_empty())
                || scoped_calls != unique
            {
                return Err(invalid(
                    "computer abandoned marker has inconsistent scoped identities",
                ));
            }
            ids.extend(scoped);
        }
    }
    Ok(ids)
}

fn computer_continuation_boundary(
    messages: &[Message],
    protocol: wire::ProtocolFamily,
) -> Result<Option<usize>, LlmError> {
    if !matches!(
        protocol,
        wire::ProtocolFamily::GeminiInteractions | wire::ProtocolFamily::OpenAiResponses
    ) {
        return Ok(None);
    }
    let Some(current) = crate::computer::current_continuation() else {
        return Ok(None);
    };
    let mut boundary = None;
    for (index, message) in messages.iter().enumerate() {
        for block in &message.content {
            if let Some(("lingxi_computer_continuation", value, _)) = computer_marker(block) {
                let saved: wire::ContinuationRef = serde_json::from_value(
                    value
                        .get("continuation")
                        .ok_or_else(|| {
                            invalid("computer continuation marker is missing its reference")
                        })?
                        .clone(),
                )
                .map_err(invalid)?;
                if saved == current {
                    boundary = Some(index);
                }
            }
        }
    }
    boundary
        .map(Some)
        .ok_or_else(|| invalid("computer continuation has no matching durable input boundary"))
}

pub(crate) fn canonical_message_content(
    message: &Message,
    family: wire::ProtocolFamily,
) -> Result<Vec<wire::ContentBlock>, LlmError> {
    let ids = computer_tool_ids(std::slice::from_ref(message))?;
    let acknowledged = acknowledged_computer_receipts(std::slice::from_ref(message))?;
    let abandoned = abandoned_computer_calls(std::slice::from_ref(message))?;
    Ok(
        project_message(message, family, &ids, &acknowledged, &abandoned, 0, false)?
            .into_iter()
            .map(|(_, block)| block)
            .collect(),
    )
}
