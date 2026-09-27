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
        ProviderId::Gemini => "gemini",
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
        projected.inference = original.inference.clone();
        projected.info = original.info.clone();
        projected.extra = original.extra.clone();
        projected.pricing = original.pricing.clone();
        for model in &mut projected.models {
            if let Some(source) = original.models.iter().find(|source| {
                source.display_model == model.display_model
                    && source.request_model == model.request_model
            }) {
                model.info = source.info.clone();
                model.pricing = source.pricing.clone();
                model.billing_mode = source.billing_mode;
            }
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
            ttl: ttl_1h.then_some(wire::CacheTtl::OneHour),
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
    if replay_companion(block).is_some() {
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
    if !gemini_family(family) {
        return None;
    }
    if value.get("type").and_then(Value::as_str) != Some("lingxi_replay_metadata") {
        return None;
    }
    let block: wire::ContentBlock = serde_json::from_value(value.get("block")?.clone()).ok()?;
    has_replay_metadata(&block).then_some((family, block))
}
fn has_replay_metadata(block: &wire::ContentBlock) -> bool {
    match block {
        wire::ContentBlock::Text {
            thought_signature, ..
        } => thought_signature.is_some(),
        wire::ContentBlock::ToolUse {
            thought_signature,
            provider_id,
            ..
        } => thought_signature.is_some() || provider_id.is_some(),
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
        .filter_map(replay_companion)
        .filter(|(protocol, _)| *protocol == family)
        .map(|(_, block)| block)
        .collect();
    let mut content = Vec::new();
    for item in &message.content {
        if skip_replay_block(item, family, skip_unsigned_reasoning)? {
            continue;
        }
        let position = metadata.iter().position(|native| match (item, native) {
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
            _ => false,
        });
        if let Some(position) = position {
            let mut native = metadata.remove(position);
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
) -> Result<wire::CompletionRequest, LlmError> {
    let mut result: wire::CompletionRequest =
        serde_json::from_value(json!({"model":req.model,"messages":[]})).map_err(invalid)?;
    result.messages = req
        .messages
        .iter()
        .map(|message| {
            Ok(wire::ConversationMessage {
                role: serde_json::from_value(json!(message.role)).map_err(invalid)?,
                content: message_content(message, protocol)?,
            })
        })
        .collect::<Result<Vec<_>, LlmError>>()?
        .into_iter()
        .filter(|message| !message.content.is_empty())
        .collect();
    result.system = req
        .system
        .iter()
        .map(|b| wire::SystemBlock {
            text: b.text.clone(),
            cacheable: b.cache_control.is_some(),
            cache_control: b.cache_control.as_ref().map(cache),
        })
        .collect();
    result.tools = req
        .tools
        .iter()
        .map(|t| wire::ToolSpec {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            strict: t.strict,
            tool_type: t.tool_type.clone(),
            defer_loading: t.defer_loading.then_some(true),
            extra: Value::Object(t.extra.clone()),
        })
        .collect();
    for tool in &mut result.tools {
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
    result.controls.response_format = req.response_format.as_ref().map(|format| match format {
        ResponseFormat::JsonObject => wire::ResponseFormat::JsonObject,
        ResponseFormat::JsonSchema { schema } => wire::ResponseFormat::JsonSchema {
            name: "response".into(),
            schema: schema.clone(),
            strict: true,
        },
    });
    result.controls.anthropic.context_hint = req.context_hint.clone();
    result.controls.responses = wire::ResponsesControls {
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
    result.previous_response_id = req
        .openai_responses
        .previous_response_id
        .as_deref()
        .map(wire::ResponseId::new);
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
        let context = self.context(&req.model, mode);
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
            if !has_replay_metadata(block) {
                continue;
            }
            let mut block = block.clone();
            if let wire::ContentBlock::ToolUse { input, .. } = &mut block {
                let Some(arguments) = self.arguments.get(index) else {
                    continue;
                };
                let Ok(value) = serde_json::from_str(arguments) else {
                    continue;
                };
                *input = value;
            }
            ready.push((*index | 0x8000_0000, companion(&block, self.family)?));
        }
        for (index, block) in ready {
            self.start(index, block, out);
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
                } => {
                    let index = wire_block_index(block)?;
                    if gemini_family(self.family) {
                        self.replay
                            .entry(index)
                            .or_insert_with(|| wire::ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: Value::Null,
                                provider_id,
                                thought_signature: None,
                            });
                        self.arguments
                            .entry(index)
                            .or_default()
                            .push_str(&arguments_fragment);
                        self.pending_tools.insert(index);
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
                    self.start(
                        (wire_block_index(block)?)
                            .checked_add(1 << 31)
                            .ok_or_else(|| invalid("native content index overflow"))?,
                        host_block(wire::ContentBlock::ProviderContent { protocol, value })?,
                        &mut out,
                    );
                }
                wire::StreamEvent::End {
                    stop_reason,
                    usage: report,
                    inference,
                } => {
                    if !self.done {
                        self.flush_replay(&mut out)?;
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
                wire::StreamEvent::Inference { .. }
                | wire::StreamEvent::WebSearch { .. }
                | wire::StreamEvent::FileSearch { .. } => {}
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
    decoded: wire::CompletionResponse,
    response: ProviderResponse,
    protocol: wire::ProtocolFamily,
) -> Result<LlmResponse, LlmError> {
    let normalized = usage(&decoded.usage, &decoded.inference)
        .map(|(u, _)| u)
        .unwrap_or_default();
    let mut content = Vec::new();
    for block in decoded.message.content {
        let replay = if gemini_family(protocol) && has_replay_metadata(&block) {
            Some(companion(&block, protocol)?)
        } else {
            None
        };
        content.push(host_block(block)?);
        content.extend(replay);
    }
    Ok(LlmResponse {
        id: decoded
            .response_id
            .map(|id| id.as_str().to_owned())
            .or(response.request_id)
            .unwrap_or_else(|| response.body_json["id"].as_str().unwrap_or_default().into()),
        model: decoded.model,
        content,
        stop_reason: Some(stop(decoded.stop_reason)),
        stop_details: response
            .body_json
            .get("stop_details")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()).map_err(invalid))
            .transpose()?,
        usage: normalized,
        cost: None,
        provider_metadata: response.body_json,
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
