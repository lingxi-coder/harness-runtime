//! Projection between LingXi's host contracts and the independent wire client.
//! Provider encoding and decoding are always delegated to lingxi-llm-client.
use crate::*;
use base64::Engine;
use lingxi_llm_client::{self as client, protocol as wire};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn invalid(error: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest {
        message: error.to_string(),
    }
}
pub(crate) fn family(protocol: &ProtocolFamily) -> wire::ProtocolFamily {
    serde_json::from_value(serde_json::to_value(protocol).expect("protocol serializes"))
        .expect("host and client protocol family mapping")
}
pub(crate) fn provider_name(provider: &ProviderId) -> &str {
    match provider {
        ProviderId::AnthropicFirstParty => "anthropic",
        ProviderId::OpenAI => "openai",
        // The host enum uses the product name; the SDK catalog and service
        // authorization scopes use Google's canonical provider identity.
        ProviderId::Gemini => "google",
        ProviderId::VertexGemini => "vertex-gemini",
        ProviderId::VertexClaude => "vertex-claude",
        ProviderId::BedrockClaude => "bedrock-claude",
        ProviderId::FoundryClaude => "foundry-claude",
        ProviderId::AzureOpenAI => "azure-openai",
        ProviderId::OpenAICompatible { name } | ProviderId::Custom { name } => name,
    }
}

pub(crate) fn profile(profile: &ProviderProfile) -> Result<wire::ProviderProfile, LlmError> {
    let mut value = json!({
        "provider_id":provider_name(&profile.provider_id), "profile_name":profile.profile_name,
        "base_url":profile.base_url,"protocol":family(&profile.protocol), "auth":"none",
        "regions":profile.regions,"connection":profile.connection,
        "supports_websockets":profile.supports_websockets,
        "websocket_connect_timeout_ms":profile.websocket_connect_timeout_ms,
        "extra":{"supports_previous_response_id":true},
        "models":profile.models.iter().map(|model| {
            let caps=model.capabilities;
            let support=|enabled| if enabled {"supported"} else {"unsupported"};
            json!({"display_model":model.display_model,"request_model":model.request_model,
                "billing_model":model.billing_model,"aliases":model.aliases,
                "description":model.description,"metadata":model.metadata,
                "capability_support":{"streaming":support(caps.streaming),"tools":support(caps.tools),
                    "vision":support(caps.vision),"documents":support(caps.documents),
                    "reasoning":support(caps.reasoning),"structured_output":support(caps.structured_output)}})
        }).collect::<Vec<_>>(),
    });
    if let Some(azure) = &profile.azure {
        value["azure"] = json!({"api_version":azure.api_version});
    }
    if let Some(signing) = &profile.signing {
        value["signing"] = serde_json::to_value(signing).map_err(invalid)?;
    }
    let mut projected: wire::ProviderProfile = serde_json::from_value(value).map_err(invalid)?;
    let builtin = if profile.wire_profile.is_none() {
        client::builtin_providers()
            .map_err(invalid)?
            .into_iter()
            .find(|candidate| {
                candidate.provider_id.as_str() == provider_name(&profile.provider_id)
                    && candidate.protocol == projected.protocol
            })
    } else {
        None
    };
    if let Some(original) = profile.wire_profile.as_ref().or(builtin.as_ref()) {
        for model in &mut projected.models {
            if let Some(source) = original.models.iter().find(|source| {
                source.display_model == model.display_model
                    && source.request_model == model.request_model
            }) {
                model.info = source.info.clone();
                model.pricing = source.pricing.clone();
                model.billing_mode = source.billing_mode;
                // The host owns its six conversation capability switches. Keep
                // newer SDK capability flags instead of resetting them to unknown.
                if let Some(host) = &mut model.capability_support {
                    if let Some(source) = &source.capability_support {
                        host.signed_reasoning = source.signed_reasoning;
                    }
                }
            }
        }
        if original.provider_id == projected.provider_id
            && original.protocol == projected.protocol
            && original.base_url.trim_end_matches('/') == projected.base_url.trim_end_matches('/')
        {
            // Preserve the SDK's independent services only for the same
            // provider identity and endpoint. A custom proxy
            // must not acquire official service endpoints and send its secret there.
            let mut merged = original.clone();
            merged.provider_id = projected.provider_id;
            merged.profile_name = projected.profile_name;
            merged.base_url = projected.base_url;
            merged.protocol = projected.protocol;
            merged.auth = projected.auth;
            merged.regions = projected.regions;
            merged.connection = projected.connection;
            merged.supports_websockets = projected.supports_websockets;
            merged.websocket_connect_timeout_ms = projected.websocket_connect_timeout_ms;
            merged.azure = projected.azure;
            merged.signing = projected.signing;
            merged.models = projected.models;
            projected = merged;
        } else {
            // These conversation defaults were already inherited before the
            // service upgrade. Dedicated service routing remains empty.
            projected.inference = original.inference.clone();
            projected.info = original.info.clone();
            projected.extra = original.extra.clone();
            projected.pricing = original.pricing.clone();
        }
    }
    projected.pricing.billing_mode = match profile.pricing.billing_mode {
        platform_api::ModelBillingMode::PerToken => wire::BillingMode::PerToken,
        platform_api::ModelBillingMode::Subscription => wire::BillingMode::Subscription,
        platform_api::ModelBillingMode::Free => wire::BillingMode::Free,
        platform_api::ModelBillingMode::Unknown => wire::BillingMode::Unknown,
    };
    for model in &mut projected.models {
        if let Some((_, price)) = profile.pricing.overrides.iter().find(|(name, _)| {
            name == &model.display_model
                || name == &model.request_model
                || name == &model.billing_model
        }) {
            model.pricing = Some(price.to_wire("override"));
            model.billing_mode = Some(wire::BillingMode::PerToken);
        }
    }
    Ok(projected)
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

fn native_family(family: wire::ProtocolFamily) -> wire::ProtocolFamily {
    match family {
        wire::ProtocolFamily::BedrockClaude
        | wire::ProtocolFamily::VertexClaude
        | wire::ProtocolFamily::FoundryClaude => wire::ProtocolFamily::AnthropicMessages,
        wire::ProtocolFamily::AzureOpenAi => wire::ProtocolFamily::OpenAiChat,
        other => other,
    }
}

fn skip_unsigned_reasoning(message: &Message, family: wire::ProtocolFamily) -> bool {
    native_family(family) == wire::ProtocolFamily::AnthropicMessages
        && message.content.iter().any(|block| {
            let ContentBlock::ProviderContent { protocol, value } = block else {
                return false;
            };
            matches!(value["type"].as_str(), Some("reasoning" | "chat_reasoning"))
                && serde_json::from_value(json!(protocol))
                    .is_ok_and(|source| native_family(source) != native_family(family))
        })
}

fn skip_replay_block(
    block: &ContentBlock,
    family: wire::ProtocolFamily,
    skip_unsigned_reasoning: bool,
) -> Result<bool, LlmError> {
    if replay_companion(block).is_some()
        || matches!(block, ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_observation")
    {
        return Ok(true);
    }
    match block {
        // Responses/Chat summaries remain visible in history, but are not
        // signed Anthropic thinking and cannot be replayed on that wire.
        ContentBlock::Reasoning {
            signature: None, ..
        } if skip_unsigned_reasoning => Ok(true),
        ContentBlock::ProviderContent { protocol, value } => {
            let source = serde_json::from_value(json!(protocol)).map_err(invalid)?;
            // Native state belongs to its wire. Keep visible text when it was
            // embedded in a native block, but never send another wire's state.
            Ok(native_family(source) != native_family(family)
                && !(value["type"].as_str() == Some("text") && value["text"].is_string()))
        }
        ContentBlock::ServerToolUse { .. }
        | ContentBlock::ConnectorText { .. }
        | ContentBlock::AdvisorToolResult { .. }
        | ContentBlock::CacheEdits { .. } => {
            Ok(native_family(family) != wire::ProtocolFamily::AnthropicMessages)
        }
        _ => Ok(false),
    }
}

fn block(
    block: &ContentBlock,
    protocol: wire::ProtocolFamily,
) -> Result<wire::ContentBlock, LlmError> {
    let claude = matches!(
        protocol,
        wire::ProtocolFamily::AnthropicMessages
            | wire::ProtocolFamily::BedrockClaude
            | wire::ProtocolFamily::VertexClaude
            | wire::ProtocolFamily::FoundryClaude
    );
    let native = |value| wire::ContentBlock::ProviderContent {
        protocol: wire::ProtocolFamily::AnthropicMessages,
        value,
    };
    let base64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(match block {
        ContentBlock::ProviderContent {
            protocol: source,
            value,
        } => {
            let source = serde_json::from_value(json!(source)).map_err(invalid)?;
            if native_family(source) == native_family(protocol) {
                wire::ContentBlock::ProviderContent {
                    protocol: native_family(source),
                    value: value.clone(),
                }
            } else {
                wire::ContentBlock::Text {
                    text: value["text"].as_str().unwrap_or_default().into(),
                    thought_signature: None,
                }
            }
        }
        ContentBlock::Text {
            text,
            cache_control,
        }
        | ContentBlock::TextJsUtf16 {
            text,
            cache_control,
            ..
        } => {
            if claude && cache_control.is_some() {
                native(
                    json!({"type":"text","text":text,"cache_control":cache(cache_control.as_ref().unwrap()).wire_value()}),
                )
            } else {
                wire::ContentBlock::Text {
                    text: text.clone(),
                    thought_signature: None,
                }
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
            let exact_text = ::protocol::js_utf16::tool_result_display(output).map(Value::String);
            let output = exact_text.as_ref().unwrap_or(output);
            if claude && (cache_control.is_some() || cache_reference.is_some()) {
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

fn gemini_family(family: wire::ProtocolFamily) -> bool {
    matches!(
        family,
        wire::ProtocolFamily::GeminiGenerateContent | wire::ProtocolFamily::VertexGemini
    )
}

// Canonical SDK metadata is kept beside the host block in the transcript.
// Rehydrate it before encoding; the companion is never sent as a second part.
fn replay_companion(block: &ContentBlock) -> Option<(wire::ProtocolFamily, wire::ContentBlock)> {
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
fn native_cited_text(block: &wire::ContentBlock) -> Option<&str> {
    let wire::ContentBlock::ProviderContent { protocol, value } = block else {
        return None;
    };
    (native_family(*protocol) == wire::ProtocolFamily::AnthropicMessages
        && value["type"] == "text"
        && value["citations"]
            .as_array()
            .is_some_and(|citations| !citations.is_empty()))
    .then(|| value["text"].as_str())
    .flatten()
}

fn has_replay_metadata(block: &wire::ContentBlock) -> bool {
    if native_cited_text(block).is_some() {
        return true;
    }
    match block {
        wire::ContentBlock::Text {
            thought_signature, ..
        } => thought_signature.is_some(),
        wire::ContentBlock::ToolUse {
            thought_signature,
            provider_id,
            caller,
            toolset_name,
            ..
        } => {
            thought_signature.is_some()
                || provider_id.is_some()
                || caller.is_some()
                || toolset_name.is_some()
        }
        _ => false,
    }
}
fn companion(
    block: &wire::ContentBlock,
    family: wire::ProtocolFamily,
) -> Result<ContentBlock, LlmError> {
    Ok(ContentBlock::ProviderContent {
        protocol: serde_json::to_value(family)
            .map_err(invalid)?
            .as_str()
            .unwrap()
            .into(),
        value: json!({"type":"lingxi_replay_metadata", "block":serde_json::to_value(block).map_err(invalid)?}),
    })
}
fn message_content(
    message: &Message,
    family: wire::ProtocolFamily,
) -> Result<Vec<wire::ContentBlock>, LlmError> {
    let skip_unsigned_reasoning = skip_unsigned_reasoning(message, family);
    let mut metadata: Vec<_> = message
        .content
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            replay_companion(block).map(|(protocol, block)| (index, protocol, block))
        })
        .filter(|(_, protocol, _)| native_family(*protocol) == native_family(family))
        .map(|(index, _, block)| (index, block))
        .collect();
    let mut content = Vec::new();
    for (item_index, item) in message.content.iter().enumerate() {
        if skip_replay_block(item, family, skip_unsigned_reasoning)? {
            continue;
        }
        let position = metadata
            .iter()
            .position(|(native_index, native)| match (item, native) {
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
                ) if *native_index == item_index + 1 => {
                    native_cited_text(native) == Some(text.as_str())
                }
                _ => false,
            });
        if let Some(position) = position {
            let (_, mut native) = metadata.remove(position);
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
            content.push(native);
        } else {
            content.push(block(item, family)?);
        }
    }
    Ok(content)
}

/// Exact host strings use the SDK's JSON override mechanism after projection.
/// Count retained blocks so dropping foreign replay metadata cannot shift an
/// override onto a different text or tool result.
pub(crate) fn message_string_overrides(
    req: &LlmRequest,
    family: wire::ProtocolFamily,
) -> Result<BTreeMap<String, Vec<u16>>, LlmError> {
    let mut overrides = BTreeMap::new();
    if native_family(family) != wire::ProtocolFamily::AnthropicMessages {
        return Ok(overrides);
    }
    let mut mi = 0;
    for message in &req.messages {
        let skip_unsigned_reasoning = skip_unsigned_reasoning(message, family);
        let mut bi = 0;
        for block in &message.content {
            if skip_replay_block(block, family, skip_unsigned_reasoning)? {
                continue;
            }
            let exact = match block {
                ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => Some(("text", utf16_code_units.clone())),
                ContentBlock::ToolResult { output, .. } => {
                    ::protocol::js_utf16::tool_result_units(output).map(|units| ("content", units))
                }
                _ => None,
            };
            if let Some((field, units)) = exact {
                overrides.insert(format!("/messages/{mi}/content/{bi}/{field}"), units);
            }
            bi += 1;
        }
        if bi > 0 {
            mi += 1;
        }
    }
    Ok(overrides)
}

pub(crate) fn request(
    req: &LlmRequest,
    protocol: wire::ProtocolFamily,
) -> Result<wire::ChatRequest, LlmError> {
    let mut result: wire::ChatRequest =
        serde_json::from_value(json!({"model":req.model,"messages":[]})).map_err(invalid)?;
    let mut message_positions = BTreeMap::new();
    for (message_index, message) in req.messages.iter().enumerate() {
        let content = message_content(message, protocol)?;
        if content.is_empty() {
            continue;
        }
        let skip_unsigned = skip_unsigned_reasoning(message, protocol);
        let mut projected_block = 0;
        for (block_index, block) in message.content.iter().enumerate() {
            if !skip_replay_block(block, protocol, skip_unsigned)? {
                message_positions.insert(
                    (message_index, block_index),
                    (result.messages.len(), projected_block),
                );
                projected_block += 1;
            }
        }
        result.messages.push(wire::ConversationMessage {
            role: serde_json::from_value(json!(message.role)).map_err(invalid)?,
            content,
            native_options: Vec::new(),
        });
    }
    let toolsets: BTreeMap<_, _> = req
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(replay_companion)
        .filter(|(source, _)| native_family(*source) == native_family(protocol))
        .filter_map(|(_, block)| match block {
            wire::ContentBlock::ToolUse {
                id,
                toolset_name: Some(toolset),
                ..
            } => Some((id, toolset)),
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
    result.system = req
        .system
        .iter()
        .map(|b| wire::SystemBlock {
            text: b.text.clone(),
        })
        .collect();
    result.hosted_tools = req.hosted_tools.clone();
    result.native_options = req.native_options.clone();
    result.prompt_cache = req.prompt_cache.clone();
    for breakpoint in &mut result.prompt_cache.breakpoints {
        if let wire::CachePosition::Message { index, block } = breakpoint.position {
            let Some(&(index, block)) = message_positions.get(&(index, block)) else {
                return Err(invalid(
                    "prompt-cache breakpoint targets content removed during protocol projection",
                ));
            };
            breakpoint.position = wire::CachePosition::Message { index, block };
        }
    }
    result.continuation = req.continuation.clone();
    // Legacy system markers now use the SDK's positional cache policy.
    if native_family(protocol) == wire::ProtocolFamily::AnthropicMessages {
        for (index, block) in req.system.iter().enumerate() {
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
    result.tools = req
        .tools
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
            match crate::strict_schema::to_strict_schema(&tool.input_schema) {
                Ok(schema) => tool.input_schema = schema,
                Err(_) => tool.strict = false,
            }
        }
    }
    result.tool_choice = match &req.tool_choice {
        Some(ToolChoice::Required) => wire::ToolChoice::Any,
        Some(ToolChoice::None) => wire::ToolChoice::None,
        Some(ToolChoice::Tool { name }) => wire::ToolChoice::Tool { name: name.clone() },
        _ => wire::ToolChoice::Auto,
    };
    result.max_tokens = req.max_tokens;
    result.temperature = req.temperature.map(|t| t as f32);
    result.stop_sequences = req.stop_sequences.clone();
    result.metadata = req
        .metadata
        .as_ref()
        .map(|m| json!({"user_id":m.user_id}))
        .unwrap_or(Value::Null);
    result.controls.top_p = req.top_p;
    let legacy_format = req.response_format.as_ref().map(|format| match format {
        ResponseFormat::JsonObject => wire::OutputFormat::JsonObject,
        ResponseFormat::JsonSchema { schema } => wire::OutputFormat::JsonSchema {
            name: "response".into(),
            schema: schema.clone(),
            strict: true,
        },
    });
    result.output_format = req.output_format.clone();
    if let Some(legacy_format) = legacy_format {
        if result.output_format != wire::OutputFormat::Text && result.output_format != legacy_format
        {
            return Err(invalid("conflicting legacy and typed output formats"));
        }
        result.output_format = legacy_format;
    }
    result.controls.anthropic.context_hint = req.context_hint.clone();
    result.controls.responses = wire::ResponsesControls {
        previous_response_id: req.openai_responses.previous_response_id.clone(),
        parallel_tool_calls: req
            .openai_responses
            .parallel_tool_calls
            .or((protocol == wire::ProtocolFamily::OpenAiResponses).then_some(false)),
        include: req.openai_responses.include.clone(),
        prompt_cache_key: req.openai_responses.prompt_cache_key.clone(),
        client_metadata: req.openai_responses.client_metadata.clone(),
        store: req
            .openai_responses
            .store
            .or((protocol == wire::ProtocolFamily::OpenAiResponses).then_some(false)),
        generate: req.openai_responses.generate,
    };
    if req.reasoning.is_some() || req.effort.is_some() {
        let mut thinking = wire::ThinkingConfig::default();
        match req.reasoning {
            Some(ReasoningConfig::Adaptive) => thinking.mode = Some(wire::ThinkingMode::Adaptive),
            Some(ReasoningConfig::Enabled { budget_tokens }) => {
                thinking.mode = Some(wire::ThinkingMode::Enabled);
                thinking.budget = Some(wire::ThinkingBudget::Tokens(budget_tokens));
            }
            None => {}
        }
        if let Some(effort) = &req.effort {
            if let Some(mode) = effort
                .as_str()
                .filter(|s| matches!(*s, "enabled" | "disabled"))
            {
                thinking.mode = Some(serde_json::from_value(json!(mode)).map_err(invalid)?);
            } else if effort.is_string() {
                thinking.effort = Some(serde_json::from_value(effort.clone()).map_err(invalid)?);
            } else if let Some(tokens) = effort.as_u64() {
                thinking.budget = Some(wire::ThinkingBudget::Tokens(
                    tokens.try_into().map_err(invalid)?,
                ));
            }
        }
        result.thinking = Some(thinking);
    }
    if protocol == wire::ProtocolFamily::OpenAiResponses
        && result.thinking.is_some()
        && !result
            .controls
            .responses
            .include
            .iter()
            .any(|value| value == "reasoning.encrypted_content")
    {
        result
            .controls
            .responses
            .include
            .push("reasoning.encrypted_content".into());
    }
    result.service_tier = match req
        .speed
        .as_deref()
        .or(req.openai_responses.service_tier.as_deref())
    {
        Some("fast" | "priority") => Some(wire::ServiceTier::Fast),
        Some("standard" | "default") => Some(wire::ServiceTier::Standard),
        None => None,
        Some(other) => return Err(invalid(format!("unsupported service tier: {other}"))),
    };
    Ok(result)
}

/// Restore host legacy controls that are intentionally outside the SDK's typed
/// request contract. Scoped continuations still pass through SDK validation.

fn upstream_metadata(metadata: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !metadata.is_object() {
        *metadata = if metadata.is_null() {
            json!({})
        } else {
            json!({"native": metadata.take()})
        };
    }
    let namespace = metadata
        .as_object_mut()
        .expect("metadata object")
        .entry("llm_client")
        .or_insert_with(|| json!({}));
    if !namespace.is_object() {
        *namespace = json!({"native": namespace.take()});
    }
    namespace.as_object_mut().expect("upstream metadata object")
}

fn append_observation(metadata: &mut Value, key: &str, value: Value) {
    let entries = upstream_metadata(metadata)
        .entry(key)
        .or_insert_with(|| json!([]));
    entries
        .as_array_mut()
        .expect("observation array")
        .push(value);
}

pub(crate) fn error(error: wire::LlmError) -> LlmError {
    use wire::LlmError as E;
    match error {
        E::Authentication { message } => LlmError::Authentication { message },
        E::PermissionDenied { message } => LlmError::PermissionDenied { message },
        E::InvalidRequest { message } => LlmError::InvalidRequest { message },
        E::RateLimited { retry_after, .. } => LlmError::RateLimited {
            retry_after,
            scope: None,
        },
        E::QuotaExceeded { .. } => LlmError::QuotaExceeded,
        E::ContextOverflow { limit, actual, .. } => LlmError::ContextOverflow {
            token_gap: actual.unwrap_or(0).saturating_sub(limit.unwrap_or(0)),
        },
        E::RequestTooLarge { .. } => LlmError::RequestTooLarge,
        E::ModelUnavailable { .. } => LlmError::ModelUnavailable,
        E::ProviderInternal { .. } => LlmError::ProviderInternal,
        E::Overloaded { .. } => LlmError::Overloaded { repeated: false },
        E::Transport { message } | E::ProviderFileProcessing { message, .. } => {
            LlmError::Transport { message }
        }
        E::TransportTimeout { message } => LlmError::TransportTimeout { message },
        E::FileUploadOutcomeUnknown { message } => LlmError::FileUploadOutcomeUnknown { message },
        E::TlsCert { message } => LlmError::tls_cert(message),
        E::StreamInterrupted { message } => LlmError::StreamInterrupted { message },
        E::CostUnavailable { message } => LlmError::CostUnavailable { message },
        E::UnsupportedCapability { message } => LlmError::UnsupportedCapability {
            capability: message,
        },
    }
}

pub(crate) fn usage(
    report: &wire::UsageReport,
    inference: &wire::InferenceReport,
) -> Option<(Usage, ModelAttemptUsageCompleteness)> {
    let counts = report.usage?;
    let completeness = if report.state == wire::UsageState::Complete {
        ModelAttemptUsageCompleteness::Complete
    } else {
        ModelAttemptUsageCompleteness::Partial
    };
    let mut metadata = json!({"upstreamUsageState":report.state});
    if report.state == wire::UsageState::Complete {
        metadata["input_tokens"] = json!(counts.input_tokens);
        metadata["output_tokens"] = json!(counts.output_tokens);
        metadata["cache_creation_input_tokens"] = json!(counts.cache_write_tokens);
        metadata["cache_read_input_tokens"] = json!(counts.cache_read_tokens);
    }
    // A partial report can already establish the expensive cache-write TTL.
    // Retain that fact without presenting default counters as final usage.
    if report.state == wire::UsageState::Complete || counts.cache_write_1h_tokens > 0 {
        metadata["cache_creation"] =
            json!({"ephemeral_1h_input_tokens":counts.cache_write_1h_tokens});
    }
    if let Some(server_tools) = counts.server_tool_usage {
        metadata["server_tool_use"] =
            serde_json::to_value(server_tools).expect("server usage serializes");
    }
    Some((
        Usage {
            billable_tokens: TokenUsage {
                input: counts.input_tokens,
                output: counts.output_tokens.saturating_sub(counts.reasoning_tokens),
                cache_write: counts.cache_write_tokens,
                cache_read: counts.cache_read_tokens,
                reasoning_output: counts.reasoning_tokens,
            },
            context_tokens: Some(counts.total()),
            provider_reported_total_tokens: Some(counts.total()),
            server_tool_use: counts.server_tool_usage.and_then(|u| {
                u.web_search_requests
                    .map(|web_search_requests| ServerToolUsage {
                        web_search_requests,
                    })
            }),
            provider_metadata: metadata,
            speed: (inference.service_tier == Some(wire::ServiceTier::Fast)).then(|| "fast".into()),
            cost_estimate: None,
        },
        completeness,
    ))
}

fn host_block(block: wire::ContentBlock) -> Result<ContentBlock, LlmError> {
    Ok(match block {
        wire::ContentBlock::Text { text, .. } => ContentBlock::Text {
            text,
            cache_control: None,
        },
        wire::ContentBlock::Thinking { text, signature } => {
            ContentBlock::Reasoning { text, signature }
        }
        wire::ContentBlock::RedactedThinking { data } => ContentBlock::RedactedThinking { data },
        wire::ContentBlock::ToolUse {
            id, name, input, ..
        } => ContentBlock::ToolCall {
            id: id.as_str().into(),
            name,
            input,
        },
        wire::ContentBlock::ProviderContent { protocol, value } => {
            if protocol == wire::ProtocolFamily::AnthropicMessages
                && matches!(
                    value["type"].as_str(),
                    Some("server_tool_use" | "connector_text" | "advisor_tool_result")
                )
                && value.as_object().is_some_and(|object| {
                    object.keys().all(|key| match value["type"].as_str() {
                        Some("server_tool_use") => {
                            matches!(key.as_str(), "type" | "id" | "name" | "input")
                        }
                        Some("connector_text") => {
                            matches!(key.as_str(), "type" | "connector_text" | "signature")
                        }
                        Some("advisor_tool_result") => matches!(
                            key.as_str(),
                            "type" | "tool_use_id" | "content" | "is_error"
                        ),
                        _ => false,
                    })
                })
            {
                serde_json::from_value(value).map_err(invalid)?
            } else {
                ContentBlock::ProviderContent {
                    protocol: serde_json::to_value(protocol)
                        .map_err(invalid)?
                        .as_str()
                        .unwrap()
                        .into(),
                    value,
                }
            }
        }
        _ => {
            return Err(LlmError::UnsupportedCapability {
                capability: "non-conversation output block".into(),
            })
        }
    })
}
fn stop(reason: wire::StopReason) -> String {
    match reason {
        wire::StopReason::EndTurn => "end_turn".into(),
        wire::StopReason::ToolUse => "tool_use".into(),
        wire::StopReason::MaxTokens => "max_tokens".into(),
        wire::StopReason::StopSequence => "stop_sequence".into(),
        wire::StopReason::Refusal => "refusal".into(),
        wire::StopReason::Other(s) => s,
    }
}

#[derive(Clone)]
pub(crate) struct Codec {
    profile: wire::ProviderProfile,
    inner: Arc<dyn client::WireCodec>,
}
impl std::fmt::Debug for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamCodec")
            .field("family", &self.profile.protocol)
            .finish()
    }
}
impl Codec {
    fn standalone(protocol: wire::ProtocolFamily, base_url: impl Into<String>) -> Self {
        let profile=serde_json::from_value(json!({"provider_id":"configured","profile_name":"configured","base_url":base_url.into(),"protocol":protocol,"auth":"none","models":[],"extra":{"supports_previous_response_id":true}})).expect("static codec profile");
        Self::new(profile)
    }
    pub(crate) fn new(profile: wire::ProviderProfile) -> Self {
        let inner: Arc<dyn client::WireCodec> = match profile.protocol {
            wire::ProtocolFamily::AnthropicMessages => Arc::new(client::AnthropicMessagesCodec),
            wire::ProtocolFamily::OpenAiChat => Arc::new(client::OpenAiChatCodec),
            wire::ProtocolFamily::OpenAiResponses => Arc::new(client::OpenAiResponsesCodec),
            wire::ProtocolFamily::GeminiGenerateContent => Arc::new(client::GeminiCodec),
            wire::ProtocolFamily::AzureOpenAi => Arc::new(client::AzureOpenAiCodec),
            wire::ProtocolFamily::BedrockClaude => Arc::new(client::BedrockClaudeCodec),
            wire::ProtocolFamily::VertexClaude => Arc::new(client::VertexClaudeCodec),
            wire::ProtocolFamily::VertexGemini => Arc::new(client::VertexGeminiCodec),
            wire::ProtocolFamily::FoundryClaude => Arc::new(client::FoundryClaudeCodec),
        };
        Self { profile, inner }
    }
    fn context(&self, model: &str, mode: client::RequestMode) -> client::CodecContext {
        if let [selected] = self.profile.models.as_slice() {
            if model.is_empty()
                || model == selected.request_model
                || model == selected.display_model
            {
                return client::CodecContext::for_model(&self.profile, selected, mode);
            }
        }
        client::CodecContext::new(&self.profile, model, mode)
    }
    fn raw_response(response: &ProviderResponse) -> client::HttpResponse {
        client::HttpResponse {
            status: response.status,
            headers: response
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body: serde_json::to_vec(&response.body_json)
                .expect("response JSON")
                .into(),
        }
    }
    fn encode(
        &self,
        req: &LlmRequest,
        mode: client::RequestMode,
    ) -> Result<ProviderRequest, LlmError> {
        let context = self
            .context(&req.model, mode)
            .with_account_scope(req.account_scope.as_deref())
            .with_file_scope(req.file_account_scope.as_deref());
        let input = request(req, self.profile.protocol)?;
        let output = self
            .inner
            .encode_request(client::EncodeRequest::new(&input), &context)
            .map_err(error)?;
        let mut result = ProviderRequest::post_json(
            output.url,
            serde_json::from_slice(&output.body).map_err(invalid)?,
        );
        result.method = output.method;
        result.headers = output.headers.into_iter().collect();
        if self.profile.protocol == wire::ProtocolFamily::BedrockClaude {
            result.stream_framing = StreamFraming::AwsEventStream;
        }
        result.json_string_overrides = message_string_overrides(req, self.profile.protocol)?;
        Ok(result)
    }
}

macro_rules! named_codec {
    ($name:ident, $family:ident) => {
        #[derive(Debug, Clone)]
        pub struct $name(Codec);
        impl $name {
            pub fn new(base_url: impl Into<String>) -> Self {
                Self(Codec::standalone(wire::ProtocolFamily::$family, base_url))
            }
            pub fn with_profile_name(mut self, name: impl Into<String>) -> Self {
                self.0.profile.profile_name = name.into();
                if let Some(source) = client::builtin_providers()
                    .expect("pinned catalog parses")
                    .into_iter()
                    .find(|p| p.profile_name == self.0.profile.profile_name)
                {
                    self.0.profile.extra = source.extra;
                    self.0.profile.inference = source.inference;
                    self.0.profile.info = source.info;
                    self.0.profile.models = source.models;
                }
                self
            }
        }
        impl WireCodec for $name {
            fn encode_request(&self, req: &LlmRequest) -> Result<ProviderRequest, LlmError> {
                self.0.encode_request(req)
            }
            fn response_usage(
                &self,
                response: &ProviderResponse,
            ) -> Option<(Usage, ModelAttemptUsageCompleteness)> {
                self.0.response_usage(response)
            }
            fn decode_response(&self, response: ProviderResponse) -> Result<LlmResponse, LlmError> {
                self.0.decode_response(response)
            }
            fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
                self.0.stream_decoder()
            }
            fn clone_box(&self) -> Box<dyn WireCodec> {
                Box::new(self.clone())
            }
        }
    };
}
named_codec!(OpenAiChatCodec, OpenAiChat);
named_codec!(OpenAiResponsesCodec, OpenAiResponses);
named_codec!(GeminiCodec, GeminiGenerateContent);
named_codec!(BedrockClaudeCodec, BedrockClaude);
named_codec!(VertexClaudeCodec, VertexClaude);
named_codec!(VertexGeminiCodec, VertexGemini);
named_codec!(FoundryClaudeCodec, FoundryClaude);

#[derive(Debug, Clone)]
pub struct AnthropicMessagesCodec(Codec);
impl AnthropicMessagesCodec {
    pub fn new(base_url: impl Into<String>, version: impl Into<String>) -> Self {
        let mut codec = Codec::standalone(wire::ProtocolFamily::AnthropicMessages, base_url);
        codec.profile.extra["api_version"] = json!(version.into());
        Self(codec)
    }
    pub fn encode_count_tokens_request(
        &self,
        req: &LlmRequest,
    ) -> Result<ProviderRequest, LlmError> {
        self.0.encode(req, client::RequestMode::CountTokens)
    }
    pub fn decode_count_tokens_response(
        &self,
        response: &ProviderResponse,
    ) -> Result<u64, LlmError> {
        if response.status >= 400 {
            return Err(self
                .0
                .decode_response(response.clone())
                .err()
                .unwrap_or(LlmError::ProviderInternal));
        }
        response.body_json["input_tokens"]
            .as_u64()
            .ok_or_else(|| invalid("token count response has no numeric input_tokens"))
    }
}
impl WireCodec for AnthropicMessagesCodec {
    fn encode_request(&self, req: &LlmRequest) -> Result<ProviderRequest, LlmError> {
        self.0.encode_request(req)
    }
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(Usage, ModelAttemptUsageCompleteness)> {
        self.0.response_usage(response)
    }
    fn decode_response(&self, response: ProviderResponse) -> Result<LlmResponse, LlmError> {
        self.0.decode_response(response)
    }
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        self.0.stream_decoder()
    }
    fn clone_box(&self) -> Box<dyn WireCodec> {
        Box::new(self.clone())
    }
}
#[derive(Debug, Clone)]
pub struct AzureOpenAiCodec(Codec);
impl AzureOpenAiCodec {
    pub fn new(base_url: impl Into<String>, version: impl Into<String>) -> Self {
        let mut codec = Codec::standalone(wire::ProtocolFamily::AzureOpenAi, base_url);
        codec.profile.azure = Some(wire::AzureConfig {
            api_version: Some(version.into()),
            deployment: None,
        });
        Self(codec)
    }
}
impl WireCodec for AzureOpenAiCodec {
    fn encode_request(&self, req: &LlmRequest) -> Result<ProviderRequest, LlmError> {
        self.0.encode_request(req)
    }
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(Usage, ModelAttemptUsageCompleteness)> {
        self.0.response_usage(response)
    }
    fn decode_response(&self, response: ProviderResponse) -> Result<LlmResponse, LlmError> {
        self.0.decode_response(response)
    }
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        self.0.stream_decoder()
    }
    fn clone_box(&self) -> Box<dyn WireCodec> {
        Box::new(self.clone())
    }
}
impl WireCodec for Codec {
    fn for_route(&self, route: &ResolvedRoute) -> Box<dyn WireCodec> {
        let mut codec = self.clone();
        codec.profile.models.retain(|model| {
            model.display_model == route.display_model && model.request_model == route.request_model
        });
        Box::new(codec)
    }
    fn encode_request(&self, req: &LlmRequest) -> Result<ProviderRequest, LlmError> {
        self.encode(
            req,
            if req.stream {
                client::RequestMode::Stream
            } else {
                client::RequestMode::Complete
            },
        )
    }
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(Usage, ModelAttemptUsageCompleteness)> {
        let context = self.context("", client::RequestMode::Complete);
        let response = Self::raw_response(response);
        usage(
            &self.inner.response_usage(&response, &context),
            &self.inner.response_inference(&response, &context),
        )
    }
    fn decode_response(&self, response: ProviderResponse) -> Result<LlmResponse, LlmError> {
        let raw = Self::raw_response(&response);
        let decoded = self
            .inner
            .decode_response(&raw, &self.context("", client::RequestMode::Complete))
            .map_err(|failure| {
                if (200..300).contains(&raw.status) {
                    if let wire::LlmError::ProviderInternal { message } = failure {
                        return invalid(message);
                    }
                }
                error(failure)
            })?;
        project_response(decoded, response, self.profile.protocol)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(Decoder {
            inner: Some(
                self.inner
                    .stream_decoder(&self.context("", client::RequestMode::Stream)),
            ),
            observation: Default::default(),
            family: self.profile.protocol,
            blocks: BTreeSet::new(),
            closed: BTreeSet::new(),
            metadata: Value::Null,
            done: false,
            started: false,
            replay: BTreeMap::new(),
            arguments: BTreeMap::new(),
            pending_tools: BTreeSet::new(),
            legacy_connectors: Default::default(),
        })
    }
    fn clone_box(&self) -> Box<dyn WireCodec> {
        Box::new(self.clone())
    }
}

fn wire_block_index(block: usize) -> Result<u32, LlmError> {
    u32::try_from(block)
        .ok()
        .filter(|index| *index < 0x8000_0000)
        .ok_or_else(|| invalid("provider output block index exceeds the host range"))
}

pub(crate) struct Decoder {
    inner: Option<Box<dyn client::StreamDecoder>>,
    observation: (wire::UsageReport, wire::InferenceReport),
    family: wire::ProtocolFamily,
    blocks: BTreeSet<u32>,
    closed: BTreeSet<u32>,
    metadata: Value,
    done: bool,
    started: bool,
    replay: BTreeMap<u32, wire::ContentBlock>,
    arguments: BTreeMap<u32, String>,
    pending_tools: BTreeSet<u32>,
    legacy_connectors: client::providers::anthropic::ConnectorTextAccumulator,
}
impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamStreamDecoder")
            .field("family", &self.family)
            .finish()
    }
}
impl Decoder {
    fn flush_replay(&mut self, out: &mut Vec<LlmEvent>) -> Result<(), LlmError> {
        let mut ready = Vec::new();
        for (index, block) in &self.replay {
            if !gemini_family(self.family) && !self.closed.contains(index) {
                continue;
            }
            if !has_replay_metadata(block) {
                continue;
            }
            let mut block = block.clone();
            if let wire::ContentBlock::ToolUse { input, .. } = &mut block {
                let Some(arguments) = self.arguments.get(index) else {
                    continue;
                };
                *input = if arguments.is_empty() && self.closed.contains(index) {
                    // A provider may close a zero-argument call without emitting
                    // input deltas. Match the visible host call's empty object
                    // only after that explicit completion boundary.
                    json!({})
                } else {
                    let Ok(value) = serde_json::from_str(arguments) else {
                        continue;
                    };
                    value
                };
            }
            ready.push((*index | 0x8000_0000, companion(&block, self.family)?));
        }
        for (index, block) in ready {
            self.start(index, block, out);
            if self.closed.contains(&(index & 0x7fff_ffff)) && self.closed.insert(index) {
                out.push(LlmEvent::ContentBlockStop { index });
            }
        }
        Ok(())
    }

    fn start(&mut self, index: u32, block: ContentBlock, out: &mut Vec<LlmEvent>) {
        if !self.started {
            self.started = true;
            out.push(LlmEvent::MessageStart {
                response: Box::new(LlmResponse {
                    id: String::new(),
                    model: String::new(),
                    content: vec![],
                    stop_reason: None,
                    stop_details: None,
                    usage: Usage::default(),
                    cost: None,
                    provider_metadata: self.metadata.clone(),
                }),
            });
        }
        if self.blocks.insert(index) {
            out.push(LlmEvent::ContentBlockStart {
                index,
                content_block: block,
            });
        }
    }
    fn events(
        &mut self,
        events: Vec<Result<wire::StreamEvent, wire::LlmError>>,
    ) -> Result<Vec<LlmEvent>, LlmError> {
        let mut out = Vec::new();
        for event in events {
            match event.map_err(error)? {
                wire::StreamEvent::BlockEnd { block } => {
                    for index in [
                        wire_block_index(block)?,
                        (wire_block_index(block)?).saturating_add(1 << 31),
                    ] {
                        if self.blocks.contains(&index) && self.closed.insert(index) {
                            out.push(LlmEvent::ContentBlockStop { index });
                        }
                    }
                }
                wire::StreamEvent::NativeDelta {
                    block,
                    protocol,
                    delta,
                } => {
                    if protocol == wire::ProtocolFamily::AnthropicMessages {
                        let projected = match delta["type"].as_str() {
                            Some("citations_delta") => Some(ContentDelta::CitationsDelta {
                                citation: delta["citation"].clone(),
                            }),
                            Some("connector_text_delta") => {
                                Some(ContentDelta::ConnectorTextDelta {
                                    connector_text: delta["connector_text"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .into(),
                                })
                            }
                            _ => None,
                        };
                        if let Some(delta) = projected {
                            if self.blocks.contains(&(wire_block_index(block)?)) {
                                out.push(LlmEvent::ContentBlockDelta {
                                    index: wire_block_index(block)?,
                                    delta,
                                });
                            }
                        }
                    }
                }
                wire::StreamEvent::Start { model, response_id } => {
                    if self.started {
                        continue;
                    }
                    self.started = true;
                    out.push(LlmEvent::MessageStart {
                        response: Box::new(LlmResponse {
                            id: response_id.map(|id| id.as_str().into()).unwrap_or_default(),
                            model,
                            content: vec![],
                            stop_reason: None,
                            stop_details: None,
                            usage: usage(&self.observation.0, &self.observation.1)
                                .map(|(u, _)| u)
                                .unwrap_or_default(),
                            cost: None,
                            provider_metadata: self.metadata.clone(),
                        }),
                    });
                }
                wire::StreamEvent::TextDelta { block, text } => {
                    let index = wire_block_index(block)?;
                    if gemini_family(self.family) {
                        let value =
                            self.replay
                                .entry(index)
                                .or_insert_with(|| wire::ContentBlock::Text {
                                    text: String::new(),
                                    thought_signature: None,
                                });
                        if let wire::ContentBlock::Text { text: buffered, .. } = value {
                            buffered.push_str(&text);
                        }
                    }
                    self.start(
                        index,
                        ContentBlock::Text {
                            text: String::new(),
                            cache_control: None,
                        },
                        &mut out,
                    );
                    out.push(LlmEvent::ContentBlockDelta {
                        index,
                        delta: ContentDelta::TextDelta { text },
                    });
                }
                wire::StreamEvent::ReasoningDelta { block, text } => {
                    let index = wire_block_index(block)?;
                    self.start(
                        index,
                        ContentBlock::Reasoning {
                            text: String::new(),
                            signature: None,
                        },
                        &mut out,
                    );
                    out.push(LlmEvent::ContentBlockDelta {
                        index,
                        delta: ContentDelta::ThinkingDelta { thinking: text },
                    });
                }
                wire::StreamEvent::ThoughtSignature { block, signature } => {
                    if let Some(
                        wire::ContentBlock::Text {
                            thought_signature, ..
                        }
                        | wire::ContentBlock::ToolUse {
                            thought_signature, ..
                        },
                    ) = self.replay.get_mut(&wire_block_index(block)?)
                    {
                        *thought_signature = Some(signature);
                        continue;
                    }
                    self.start(
                        wire_block_index(block)?,
                        ContentBlock::Reasoning {
                            text: String::new(),
                            signature: None,
                        },
                        &mut out,
                    );
                    out.push(LlmEvent::ContentBlockDelta {
                        index: wire_block_index(block)?,
                        delta: ContentDelta::SignatureDelta { signature },
                    });
                }
                wire::StreamEvent::RedactedThinking { block, data } => self.start(
                    wire_block_index(block)?,
                    ContentBlock::RedactedThinking { data },
                    &mut out,
                ),
                wire::StreamEvent::ToolCallDelta {
                    block,
                    id,
                    name,
                    arguments_fragment,
                    provider_id,
                    caller,
                    toolset_name,
                } => {
                    let index = wire_block_index(block)?;
                    if gemini_family(self.family)
                        || caller.is_some()
                        || toolset_name.is_some()
                        || provider_id.is_some()
                    {
                        self.replay
                            .entry(index)
                            .or_insert_with(|| wire::ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: Value::Null,
                                provider_id,
                                caller,
                                toolset_name,
                                thought_signature: None,
                            });
                        self.arguments
                            .entry(index)
                            .or_default()
                            .push_str(&arguments_fragment);
                        if gemini_family(self.family) {
                            self.pending_tools.insert(index);
                        }
                    }
                    self.start(
                        index,
                        ContentBlock::ToolCall {
                            id: id.as_str().into(),
                            name,
                            input: json!({}),
                        },
                        &mut out,
                    );
                    if !arguments_fragment.is_empty() {
                        out.push(LlmEvent::ContentBlockDelta {
                            index,
                            delta: ContentDelta::InputJsonDelta {
                                partial_json: arguments_fragment,
                            },
                        });
                    }
                }
                wire::StreamEvent::ProviderContent {
                    block,
                    protocol,
                    value,
                } => {
                    let index = wire_block_index(block)?;
                    let native = wire::ContentBlock::ProviderContent { protocol, value };
                    let projected = if let Some(text) = native_cited_text(&native) {
                        // The SDK emits both display deltas and complete cited
                        // replay data for this same provider block. Keep the
                        // native payload as metadata for the visible text.
                        if !self.blocks.contains(&index) {
                            self.start(
                                index,
                                ContentBlock::Text {
                                    text: String::new(),
                                    cache_control: None,
                                },
                                &mut out,
                            );
                            out.push(LlmEvent::ContentBlockDelta {
                                index,
                                delta: ContentDelta::TextDelta { text: text.into() },
                            });
                        }
                        companion(&native, protocol)?
                    } else {
                        host_block(native)?
                    };
                    self.start(index | 0x8000_0000, projected, &mut out);
                }
                wire::StreamEvent::End {
                    stop_reason,
                    usage: report,
                    inference,
                } => {
                    if !self.done {
                        self.flush_replay(&mut out)?;
                        if usage(&report, &inference).is_none() && !self.metadata.is_null() {
                            // Observations can exist without billable token measurements.
                            // Preserve them in a host-only transcript companion rather
                            // than fabricating a zero-usage report. Replay filters this tag.
                            let index = (0..=u32::MAX)
                                .rev()
                                .find(|index| !self.blocks.contains(index))
                                .ok_or_else(|| {
                                    invalid("no stream index available for provider observations")
                                })?;
                            self.start(index, ContentBlock::ProviderContent {
                                protocol: serde_json::to_value(self.family).map_err(invalid)?.as_str().unwrap().into(),
                                value: json!({"type":"lingxi_observation", "metadata": self.metadata}),
                            }, &mut out);
                        }
                        self.done = true;
                        for index in self.blocks.difference(&self.closed) {
                            out.push(LlmEvent::ContentBlockStop { index: *index });
                        }
                        out.push(LlmEvent::MessageDelta {
                            delta: MessageDeltaPayload {
                                stop_reason: Some(stop(stop_reason)),
                                stop_details: None,
                            },
                            usage: usage(&report, &inference).map(|(mut u, _)| {
                                if !self.metadata.is_null() {
                                    u.provider_metadata["stream"] = self.metadata.clone();
                                }
                                u
                            }),
                        });
                        out.push(LlmEvent::MessageStop);
                    }
                }
                wire::StreamEvent::ProviderEvent { protocol, payload } => {
                    if protocol == wire::ProtocolFamily::AnthropicMessages {
                        if let Some((index, block)) = self.legacy_connectors.push(&payload) {
                            let index = wire_block_index(usize::try_from(index).map_err(invalid)?)?;
                            self.start(index | 0x8000_0000, host_block(block)?, &mut out);
                        }
                    }
                    append_observation(
                        &mut self.metadata,
                        "provider_events",
                        json!({"protocol": protocol, "payload": payload}),
                    );
                }
                wire::StreamEvent::WebSearch { result } => {
                    out.push(LlmEvent::WebSearch {
                        result: result.clone(),
                    });
                    append_observation(
                        &mut self.metadata,
                        "web_search",
                        serde_json::to_value(result).map_err(invalid)?,
                    );
                }
                wire::StreamEvent::FileSearch { result } => {
                    append_observation(
                        &mut self.metadata,
                        "file_search",
                        serde_json::to_value(result).map_err(invalid)?,
                    );
                }
                wire::StreamEvent::Inference { .. } => {}
            }
        }
        if !self.done {
            self.flush_replay(&mut out)?;
            for index in std::mem::take(&mut self.pending_tools) {
                for index in [index, index | 0x8000_0000] {
                    if self.blocks.contains(&index) && self.closed.insert(index) {
                        out.push(LlmEvent::ContentBlockStop { index });
                    }
                }
            }
        }
        Ok(out)
    }
}
impl StreamDecoder for Decoder {
    fn observed_usage(&self) -> Option<(Usage, ModelAttemptUsageCompleteness)> {
        usage(&self.observation.0, &self.observation.1)
    }
    fn set_provider_metadata(&mut self, metadata: Value) {
        self.metadata = metadata;
    }
    fn decode_frame(&mut self, frame: RawStreamFrame) -> Result<Vec<LlmEvent>, LlmError> {
        let bytes = if self.family == wire::ProtocolFamily::BedrockClaude {
            frame.bytes
        } else {
            let mut bytes = b"data: ".to_vec();
            bytes.extend(frame.bytes);
            bytes.extend(b"\n\n");
            bytes
        };
        let inner = self.inner.as_mut().expect("codec decoder");
        let events = inner.push_bytes(&bytes);
        self.observation = (inner.usage_report(), inner.inference_report());
        self.events(events)
    }
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Vec::new());
        };
        let events = inner.finish();
        self.observation = (inner.usage_report(), inner.inference_report());
        self.events(events)
    }
}

pub(crate) fn project_response(
    decoded: wire::ChatResponse,
    response: ProviderResponse,
    protocol: wire::ProtocolFamily,
) -> Result<LlmResponse, LlmError> {
    crate::execution::validate_response_content(&decoded, &response.body_json, protocol)?;
    let normalized = usage(&decoded.usage, &decoded.inference)
        .map(|(u, _)| u)
        .unwrap_or_default();
    let mut content = Vec::new();
    for block in decoded.message.content {
        let replay = if has_replay_metadata(&block) {
            Some(companion(&block, protocol)?)
        } else {
            None
        };
        if let Some(text) = native_cited_text(&block) {
            content.push(ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            });
        } else {
            content.push(host_block(block)?);
        }
        content.extend(replay);
    }
    let mut metadata = response.body_json;
    for (key, value) in [
        ("web_search", serde_json::to_value(&decoded.web_search)),
        ("file_search", serde_json::to_value(&decoded.file_search)),
        (
            "native_metadata",
            serde_json::to_value(
                (!decoded.native_metadata.is_empty()).then_some(&decoded.native_metadata),
            ),
        ),
        (
            "response_cache",
            serde_json::to_value(&decoded.response_cache),
        ),
        ("continuation", serde_json::to_value(&decoded.continuation)),
    ] {
        let value = value.map_err(invalid)?;
        if !value.is_null() {
            upstream_metadata(&mut metadata).insert(key.into(), value);
        }
    }
    Ok(LlmResponse {
        id: decoded
            .response_id
            .map(|id| id.as_str().to_owned())
            .or(response.request_id)
            .unwrap_or_else(|| metadata["id"].as_str().unwrap_or_default().into()),
        model: decoded.model,
        content,
        stop_reason: Some(stop(decoded.stop_reason)),
        stop_details: metadata
            .get("stop_details")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()).map_err(invalid))
            .transpose()?,
        usage: normalized,
        cost: None,
        provider_metadata: metadata,
    })
}

impl Decoder {
    pub(crate) fn projection(family: wire::ProtocolFamily, metadata: Value) -> Self {
        Self {
            inner: None,
            observation: Default::default(),
            family,
            blocks: BTreeSet::new(),
            closed: BTreeSet::new(),
            metadata,
            done: false,
            started: false,
            replay: BTreeMap::new(),
            arguments: BTreeMap::new(),
            pending_tools: BTreeSet::new(),
            legacy_connectors: Default::default(),
        }
    }
    pub(crate) fn project_batch(
        &mut self,
        batch: client::StreamBatch,
    ) -> Result<Vec<LlmEvent>, LlmError> {
        self.observation = (batch.usage, batch.inference);
        self.events(batch.events)
    }
}

#[cfg(test)]
mod upgrade_tests {
    use super::*;
    use lingxi_llm_client::providers::anthropic::{
        native::AnthropicHostedTool,
        types::{
            AnthropicCodeExecutionConfig, AnthropicSkillRef, AnthropicSkillScope,
            AnthropicWebFetchConfig,
        },
    };

    fn anthropic_codec() -> Codec {
        Codec::new(
            serde_json::from_value(json!({
                "provider_id": "anthropic", "profile_name": "anthropic",
                "base_url": "https://api.anthropic.com", "protocol": "anthropic_messages",
                "auth": "none", "models": [], "extra": {"web_search": "anthropic"}
            }))
            .unwrap(),
        )
    }

    #[test]
    fn provider_native_request_options_are_projected_losslessly() {
        let extension = wire::NativeExtension::new(
            "anthropic.request_options.v1",
            json!({"client_toolsets": [], "future_policy": {"preserve": true}}),
        )
        .unwrap();
        let mut req = LlmRequest::new("claude-sonnet-4-6").with_user_text("Hello");
        req.native_options.push(extension.clone());
        let projected = request(&req, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert_eq!(projected.native_options, vec![extension]);
        assert_eq!(
            projected.native_options[0].data()["future_policy"]["preserve"],
            true
        );
    }

    #[test]
    fn legacy_tool_caller_policy_uses_the_provider_native_options_contract() {
        use lingxi_llm_client::providers::anthropic::types::AnthropicToolCaller;
        let mut req = LlmRequest::new("claude-sonnet-4-6");
        req.tools.push(ToolDeclaration {
            name: "lookup".into(),
            input_schema: json!({"type":"object"}),
            extra: serde_json::from_value(json!({"allowed_callers":["direct"]})).unwrap(),
            ..Default::default()
        });
        let projected = request(&req, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert_eq!(
            projected.tools[0].anthropic_allowed_callers(),
            &[AnthropicToolCaller::Direct]
        );
        assert_eq!(
            projected.tools[0].native_options[0].format(),
            "anthropic.tool_options.v1"
        );
        assert!(projected.tools[0].extra.get("allowed_callers").is_none());
    }

    #[test]
    fn response_native_metadata_keeps_its_format_and_unknown_fields() {
        let extension = wire::NativeExtension::new(
            "vendor.response_metadata.v1",
            json!({
                "resource": "resource-1", "future": {"opaque": [1, 2, 3]}
            }),
        )
        .unwrap();
        let decoded: wire::ChatResponse = serde_json::from_value(json!({
            "message":{"role":"assistant","content":[]}, "model":"m", "stop_reason":"end_turn",
            "usage":wire::UsageReport::default(), "native_metadata":[extension]
        }))
        .unwrap();
        let response = project_response(
            decoded,
            ProviderResponse::json(200, json!({"content":[]})),
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(
            response.provider_metadata["llm_client"]["native_metadata"],
            serde_json::to_value(vec![extension]).unwrap()
        );
    }

    #[test]
    fn hosted_search_fetch_and_remote_skills_reach_the_native_encoder() {
        let mut req =
            LlmRequest::new("claude-sonnet-4-6").with_user_text("Read the source and make a PDF");
        req.max_tokens = Some(1024);
        req.hosted_tools = vec![
            wire::HostedTool::WebSearch(wire::WebSearchConfig::default()),
            AnthropicHostedTool::WebFetch(AnthropicWebFetchConfig::default()).into(),
            AnthropicHostedTool::CodeExecution(AnthropicCodeExecutionConfig {
                skills: vec![AnthropicSkillRef::anthropic("pdf")],
                ..Default::default()
            })
            .into(),
        ];
        let encoded = anthropic_codec().encode_request(&req).unwrap();
        let tools = encoded.body_json["tools"].as_array().unwrap();
        for name in ["web_search", "web_fetch", "code_execution"] {
            assert!(tools.iter().any(|tool| tool["name"] == name), "{encoded:?}");
        }
        assert_eq!(
            encoded.body_json["container"]["skills"][0]["skill_id"],
            "pdf"
        );
        assert!(encoded.body_json.get("hosted_tools").is_none());
    }

    #[test]
    fn custom_remote_skill_requires_the_matching_host_account_scope() {
        let scope =
            AnthropicSkillScope::new("anthropic", "https://api.anthropic.com", "workspace-a")
                .unwrap();
        let mut req = LlmRequest::new("claude-sonnet-4-6").with_user_text("Run the skill");
        req.max_tokens = Some(1024);
        req.hosted_tools.push(
            AnthropicHostedTool::CodeExecution(AnthropicCodeExecutionConfig {
                skills: vec![AnthropicSkillRef::custom("skill_example", scope)],
                ..Default::default()
            })
            .into(),
        );
        req.account_scope = Some("workspace-b".into());
        assert!(anthropic_codec().encode_request(&req).is_err());
        req.account_scope = Some("workspace-a".into());
        assert!(anthropic_codec().encode_request(&req).is_ok());
        let roundtrip: LlmRequest =
            serde_json::from_value(serde_json::to_value(req).unwrap()).unwrap();
        assert!(roundtrip.account_scope.is_none());
    }

    #[test]
    fn legacy_system_cache_scope_and_ttl_survive_typed_cache_projection() {
        let mut req = LlmRequest::new("claude-sonnet-4-6").with_user_text("Hello");
        req.max_tokens = Some(1024);
        req.system.push(SystemBlock {
            text: "System".into(),
            cache_control: Some(CacheControl::EphemeralScoped {
                scope: Some(CacheScope::Global),
                ttl_1h: true,
            }),
        });
        let encoded = anthropic_codec().encode_request(&req).unwrap();
        assert_eq!(
            encoded.body_json["system"][0]["cache_control"],
            json!({"type":"ephemeral", "ttl":"1h", "scope":"global"})
        );
    }

    #[test]
    fn native_programmatic_caller_metadata_survives_transcript_replay() {
        let native = wire::ContentBlock::ToolUse {
            id: wire::ToolUseId::new("tool-1"),
            name: "lookup".into(),
            input: json!({"key":"value"}),
            provider_id: None,
            caller: Some(json!({"type":"code_execution_20260120","tool_id":"server-1"})),
            toolset_name: None,
            thought_signature: None,
        };
        let message = Message {
            role: "assistant".into(),
            content: vec![
                host_block(native.clone()).unwrap(),
                companion(&native, wire::ProtocolFamily::AnthropicMessages).unwrap(),
            ],
        };
        let restored = message_content(&message, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert_eq!(restored, vec![native]);
        let foreign = message_content(&message, wire::ProtocolFamily::OpenAiChat).unwrap();
        assert!(matches!(
            &foreign[0],
            wire::ContentBlock::ToolUse { caller: None, .. }
        ));
    }

    #[test]
    fn search_observations_and_server_usage_survive_stream_projection() {
        let mut decoder = Decoder::projection(wire::ProtocolFamily::AnthropicMessages, Value::Null);
        let usage = wire::UsageReport::measured(
            wire::Usage {
                server_tool_usage: Some(wire::ServerToolUsage {
                    web_fetch_requests: Some(2),
                    ..Default::default()
                }),
                ..Default::default()
            },
            wire::UsageState::Complete,
        );
        let events = decoder.events(vec![
            Ok(wire::StreamEvent::Start { model: "m".into(), response_id: None }),
            Ok(wire::StreamEvent::WebSearch { result: wire::WebSearchResult {
                citations: vec![wire::WebCitation { url: "https://example.com".into(), title: Some("Source".into()) }],
                metadata: json!({"citations":[{"start":0,"end":10}]}),
            }}),
            Ok(wire::StreamEvent::ProviderEvent { protocol: wire::ProtocolFamily::AnthropicMessages, payload: json!({"type":"message_delta","container":{"id":"container-1"}}) }),
            Ok(wire::StreamEvent::ProviderContent { block: 0, protocol: wire::ProtocolFamily::AnthropicMessages, value: json!({"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"test"}}) }),
            Ok(wire::StreamEvent::End { stop_reason: wire::StopReason::EndTurn, usage, inference: Default::default() }),
        ]).unwrap();
        assert!(!events.iter().any(|event| matches!(
            event,
            LlmEvent::ContentBlockStart {
                content_block: ContentBlock::ToolCall { .. },
                ..
            }
        )));
        let usage = events
            .iter()
            .find_map(|event| match event {
                LlmEvent::MessageDelta { usage, .. } => usage.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            usage.provider_metadata["server_tool_use"]["web_fetch_requests"],
            2
        );
        let metadata = &usage.provider_metadata["stream"]["llm_client"];
        assert_eq!(
            metadata["web_search"][0]["citations"][0]["url"],
            "https://example.com"
        );
        assert_eq!(
            metadata["provider_events"][0]["payload"]["container"]["id"],
            "container-1"
        );
    }

    #[test]
    fn native_hosted_extensions_are_not_lost_to_legacy_server_tool_types() {
        let value = json!({"type":"server_tool_use","id":"server-1","name":"web_fetch","input":{},"caller":{"type":"code_execution_20260120","tool_id":"exec-1"}});
        let projected = host_block(wire::ContentBlock::ProviderContent {
            protocol: wire::ProtocolFamily::AnthropicMessages,
            value: value.clone(),
        })
        .unwrap();
        assert!(matches!(&projected, ContentBlock::ProviderContent { .. }));
        let restored = block(&projected, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert_eq!(
            restored,
            wire::ContentBlock::ProviderContent {
                protocol: wire::ProtocolFamily::AnthropicMessages,
                value,
            }
        );
    }

    #[test]
    fn caller_metadata_does_not_finish_anthropic_tools_before_block_end() {
        let mut decoder = Decoder::projection(wire::ProtocolFamily::AnthropicMessages, Value::Null);
        let delta = |fragment: &str| {
            Ok(wire::StreamEvent::ToolCallDelta {
                block: 0,
                id: wire::ToolUseId::new("call-1"),
                name: "lookup".into(),
                provider_id: None,
                caller: Some(json!({"type":"code_execution_20260120","tool_id":"exec-1"})),
                toolset_name: None,
                arguments_fragment: fragment.into(),
            })
        };
        for fragment in ["", "{\"key\":", "\"value\"}"] {
            let events = decoder.events(vec![delta(fragment)]).unwrap();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, LlmEvent::ContentBlockStop { .. })),
                "{events:?}"
            );
        }
        let events = decoder
            .events(vec![Ok(wire::StreamEvent::BlockEnd { block: 0 })])
            .unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, LlmEvent::ContentBlockStop { index: 0 })));
        let companion = events
            .iter()
            .find_map(|event| match event {
                LlmEvent::ContentBlockStart { content_block, .. } => {
                    replay_companion(content_block).map(|(_, block)| block)
                }
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(companion, wire::ContentBlock::ToolUse { input, caller: Some(_), .. } if input == json!({"key":"value"}))
        );
    }

    #[test]
    fn closed_zero_argument_tools_keep_caller_and_toolset_metadata() {
        for arguments in ["", "{"] {
            let mut decoder =
                Decoder::projection(wire::ProtocolFamily::AnthropicMessages, Value::Null);
            let opening = decoder
                .events(vec![Ok(wire::StreamEvent::ToolCallDelta {
                    block: 0,
                    id: wire::ToolUseId::new("call-empty"),
                    name: "inspect".into(),
                    provider_id: None,
                    caller: Some(json!({"type":"direct"})),
                    toolset_name: Some("browser".into()),
                    arguments_fragment: arguments.into(),
                })])
                .unwrap();
            assert!(!opening.iter().any(|event| matches!(event,
                LlmEvent::ContentBlockStart { content_block, .. } if replay_companion(content_block).is_some())));
            let closed = decoder
                .events(vec![Ok(wire::StreamEvent::BlockEnd { block: 0 })])
                .unwrap();
            let replay = closed.iter().find_map(|event| match event {
                LlmEvent::ContentBlockStart { content_block, .. } => {
                    replay_companion(content_block).map(|(_, block)| block)
                }
                _ => None,
            });
            if arguments.is_empty() {
                assert!(matches!(replay, Some(wire::ContentBlock::ToolUse {
                    input, caller: Some(caller), toolset_name: Some(toolset), ..
                }) if input == json!({}) && caller == json!({"type":"direct"}) && toolset == "browser"));
            } else {
                assert!(
                    replay.is_none(),
                    "malformed nonempty input must not become an empty object"
                );
            }
        }
    }

    #[test]
    fn toolset_identity_is_echoed_on_the_matching_host_tool_result() {
        let native = wire::ContentBlock::ToolUse {
            id: wire::ToolUseId::new("call-1"),
            name: "click".into(),
            input: json!({}),
            provider_id: None,
            caller: None,
            toolset_name: Some("browser".into()),
            thought_signature: None,
        };
        let mut req = LlmRequest::new("claude-sonnet-4-6");
        req.messages.push(Message {
            role: "assistant".into(),
            content: vec![
                host_block(native.clone()).unwrap(),
                companion(&native, wire::ProtocolFamily::AnthropicMessages).unwrap(),
            ],
        });
        req.messages.push(Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "call-1".into(),
                output: json!("clicked"),
                is_error: false,
                cache_control: None,
                cache_reference: None,
            }],
        });
        let projected = request(&req, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert!(
            matches!(&projected.messages[1].content[0], wire::ContentBlock::ToolResult { toolset_name: Some(name), .. } if name == "browser")
        );
    }

    #[test]
    fn cache_breakpoints_follow_projection_and_reject_removed_metadata() {
        let native = wire::ContentBlock::ToolUse {
            id: wire::ToolUseId::new("call-1"),
            name: "lookup".into(),
            input: json!({}),
            provider_id: None,
            caller: Some(json!({"type":"direct"})),
            toolset_name: None,
            thought_signature: None,
        };
        let mut req = LlmRequest::new("claude-sonnet-4-6");
        req.messages.push(Message {
            role: "assistant".into(),
            content: vec![],
        });
        req.messages.push(Message {
            role: "assistant".into(),
            content: vec![
                host_block(native.clone()).unwrap(),
                companion(&native, wire::ProtocolFamily::AnthropicMessages).unwrap(),
                ContentBlock::Text {
                    text: "retained".into(),
                    cache_control: None,
                },
            ],
        });
        req.prompt_cache.breakpoints.push(wire::CacheBreakpoint {
            scope: None,
            position: wire::CachePosition::Message { index: 1, block: 2 },
            ttl: wire::CacheTtl::FiveMinutes,
        });
        let projected = request(&req, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert_eq!(
            projected.prompt_cache.breakpoints[0].position,
            wire::CachePosition::Message { index: 0, block: 1 }
        );
        req.prompt_cache.breakpoints[0].position =
            wire::CachePosition::Message { index: 1, block: 1 };
        assert!(request(&req, wire::ProtocolFamily::AnthropicMessages).is_err());
    }

    #[test]
    fn search_observations_without_usage_are_retained_but_never_replayed() {
        let mut decoder = Decoder::projection(wire::ProtocolFamily::OpenAiResponses, Value::Null);
        let events = decoder
            .events(vec![
                Ok(wire::StreamEvent::WebSearch {
                    result: wire::WebSearchResult {
                        citations: vec![wire::WebCitation {
                            url: "https://example.com".into(),
                            title: None,
                        }],
                        metadata: Value::Null,
                    },
                }),
                Ok(wire::StreamEvent::End {
                    stop_reason: wire::StopReason::EndTurn,
                    usage: wire::UsageReport::default(),
                    inference: Default::default(),
                }),
            ])
            .unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, LlmEvent::MessageDelta { usage: None, .. })));
        let observation = events
            .iter()
            .find_map(|event| match event {
                LlmEvent::ContentBlockStart {
                    content_block: block @ ContentBlock::ProviderContent { value, .. },
                    ..
                } if value["type"] == "lingxi_observation" => Some(block.clone()),
                _ => None,
            })
            .unwrap();
        if let ContentBlock::ProviderContent { value, .. } = &observation {
            assert_eq!(
                value["metadata"]["llm_client"]["web_search"][0]["citations"][0]["url"],
                "https://example.com"
            );
        }
        let replay = message_content(
            &Message {
                role: "assistant".into(),
                content: vec![observation],
            },
            wire::ProtocolFamily::OpenAiResponses,
        )
        .unwrap();
        assert!(replay.is_empty());
    }

    #[test]
    fn gemini_keeps_google_identity_and_services_only_on_the_official_endpoint() {
        let mut host = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "gemini")
            .unwrap();
        let source = host.wire_profile.as_ref().unwrap();
        assert_eq!(source.provider_id.as_str(), "google");
        let expected_embeddings = serde_json::to_value(&source.embeddings).unwrap();
        let expected_interactions = serde_json::to_value(&source.interactions).unwrap();
        let expected_file_search = serde_json::to_value(&source.gemini_file_search).unwrap();
        for explicit_source in [true, false] {
            if !explicit_source {
                host.wire_profile = None;
            }
            let projected = profile(&host).unwrap();
            assert_eq!(projected.provider_id.as_str(), "google");
            assert_eq!(
                serde_json::to_value(&projected.embeddings).unwrap(),
                expected_embeddings
            );
            assert_eq!(
                serde_json::to_value(&projected.interactions).unwrap(),
                expected_interactions
            );
            assert_eq!(
                serde_json::to_value(&projected.gemini_file_search).unwrap(),
                expected_file_search
            );
            let mut proxy = host.clone();
            proxy.base_url = "https://gemini-proxy.example/v1beta".into();
            let projected = profile(&proxy).unwrap();
            assert_eq!(projected.embeddings, Default::default());
            assert_eq!(projected.interactions, Default::default());
            assert_eq!(projected.gemini_file_search, Default::default());
        }
    }

    #[test]
    fn independent_service_routes_survive_only_trusted_profile_projection() {
        let mut host = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|p| p.provider_id == ProviderId::OpenAI)
            .unwrap();
        let source = host.wire_profile.clone().unwrap();
        let projected = profile(&host).unwrap();
        assert_eq!(
            serde_json::to_value(&projected.audio).unwrap(),
            serde_json::to_value(&source.audio).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&projected.embeddings).unwrap(),
            serde_json::to_value(&source.embeddings).unwrap()
        );
        host.wire_profile = None;
        host.base_url = "https://proxy.example/v1".into();
        let proxy = profile(&host).unwrap();
        assert_eq!(proxy.audio, Default::default());
        assert_eq!(proxy.embeddings, Default::default());
    }
}
