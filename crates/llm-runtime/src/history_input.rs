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
use std::collections::BTreeMap;
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
    Ok(project_message(message, family)?
        .into_iter()
        .map(|(_, block)| block)
        .collect())
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
    for message in messages {
        let projected = project_message(message, protocol)?;
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
}
