//! Projection between LingXi's host contracts and the independent wire client.
//! Provider encoding and decoding are always delegated to lingxi-llm-client.
#[cfg(test)]
use crate::convert::input_projection::{message_content, replay_companion};
#[cfg(test)]
use crate::history_projection::{HistoryProjector as Decoder, project_response};
use crate::*;
use lingxi_llm_client::{self as client, protocol as wire};
#[cfg(test)]
use serde_json::Value;
use serde_json::json;

fn invalid(error: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest {
        message: error.to_string(),
    }
}
pub(crate) fn family(protocol: &ProtocolFamily) -> wire::ProtocolFamily {
    *protocol
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
            if let Some(source) = original
                .models
                .iter()
                .find(|source| {
                    source.display_model == model.display_model
                        && source.request_model == model.request_model
                })
                .or_else(|| {
                    // Display labels may be changed by the host. Directory facts
                    // belong to a wire model, but duplicate wire rows still need
                    // their exact display identity to select the correct row.
                    let mut matches = original
                        .models
                        .iter()
                        .filter(|source| source.request_model == model.request_model);
                    let source = matches.next()?;
                    matches.next().is_none().then_some(source)
                })
            {
                model.info = source.info.clone();
                model.foundry = source.foundry.clone();
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
    if let Some(mode) = profile.pricing.billing_mode {
        projected.pricing.billing_mode = match mode {
            lingxi_core::host::ModelBillingMode::PerToken => wire::BillingMode::PerToken,
            lingxi_core::host::ModelBillingMode::Subscription => wire::BillingMode::Subscription,
            lingxi_core::host::ModelBillingMode::Free => wire::BillingMode::Free,
            lingxi_core::host::ModelBillingMode::Unknown => wire::BillingMode::Unknown,
        };
    }
    for model in &mut projected.models {
        if let Some((_, price)) = profile.pricing.overrides.iter().find(|(name, _)| {
            name == &model.display_model
                || name == &model.request_model
                || name == &model.billing_model
        }) {
            model.pricing = Some(price.to_sdk().with_fixed_standard_override());
            model.billing_mode = Some(wire::BillingMode::PerToken);
        }
    }
    Ok(projected)
}

/// SDK protocol adaptation plus reindexing of host-owned exact-string sidecars.
pub(crate) fn adapt_request(
    request: &LlmRequest,
    source: wire::ProtocolFamily,
    target: wire::ProtocolFamily,
) -> Result<LlmRequest, LlmError> {
    let projection = client::replay::adapt_request(
        &request.input,
        source,
        target,
        client::replay::ReplayPolicy::DropIncompatible,
    )
    .map_err(error)?;
    let mut adapted = request.clone();
    adapted.input = projection.request;
    adapted.execution.input_protocol = Some(target);
    adapted.execution.message_json_string_overrides.clear();
    for (path, units) in &request.execution.message_json_string_overrides {
        let Some(path) = path.strip_prefix("/messages/") else {
            continue;
        };
        let Some((message, path)) = path.split_once("/content/") else {
            continue;
        };
        let Some((block, field)) = path.split_once('/') else {
            continue;
        };
        let (Ok(message), Ok(block)) = (message.parse::<usize>(), block.parse::<usize>()) else {
            continue;
        };
        if let Some((message, block)) = projection.block_positions.get(&(message, block)) {
            adapted.execution.message_json_string_overrides.insert(
                format!("/messages/{message}/content/{block}/{field}"),
                units.clone(),
            );
        }
    }
    Ok(adapted)
}

/// Canonical requests need no model DTO projection. Responses host defaults are
/// explicit per-call controls; preserving them does not encode provider JSON.
pub(crate) fn request(
    req: &LlmRequest,
    protocol: wire::ProtocolFamily,
) -> Result<wire::ChatRequest, LlmError> {
    let mut input = req.input.clone();
    if protocol == wire::ProtocolFamily::OpenAiResponses {
        input
            .controls
            .responses
            .parallel_tool_calls
            .get_or_insert(false);
        input.controls.responses.store.get_or_insert(false);
        if input.thinking.is_some()
            && !input
                .controls
                .responses
                .include
                .iter()
                .any(|value| value == "reasoning.encrypted_content")
        {
            input
                .controls
                .responses
                .include
                .push("reasoning.encrypted_content".into());
        }
    }
    Ok(input)
}

/// Translate SDK failures into the host execution error contract.
pub(crate) fn error(error: wire::LlmError) -> LlmError {
    use wire::LlmError as E;
    match error {
        E::ProviderResponse {
            status,
            request_id,
            body,
            classification,
            retry_after,
        } => {
            use wire::LlmErrorKind as K;
            let message = format!(
                "{status} {}",
                json!({"error":body.get("error").cloned().unwrap_or_else(|| json!({"message":body.to_string()})),
                "provider_response":{"status":status,"request_id":request_id,"body":body,"classification":classification,"retry_after_ms":retry_after.map(|delay| delay.as_millis())}})
            );
            match classification {
                K::Authentication => LlmError::Authentication { message },
                K::PermissionDenied => LlmError::PermissionDenied { message },
                K::InvalidRequest => LlmError::InvalidRequest { message },
                K::RateLimited => LlmError::RateLimited {
                    retry_after,
                    scope: None,
                },
                K::QuotaExceeded => LlmError::QuotaExceeded,
                K::ContextOverflow => LlmError::ContextOverflow { token_gap: 0 },
                K::RequestTooLarge => LlmError::RequestTooLarge,
                K::ModelUnavailable => LlmError::ModelUnavailable,
                K::ProviderInternal => LlmError::ProviderInternal,
                K::ProviderTimeout => LlmError::ProviderTimeout {
                    message,
                    status: Some(status),
                },
                K::Overloaded => LlmError::Overloaded { repeated: false },
                K::Transport | K::ProviderFileProcessing => LlmError::Transport { message },
                K::TransportTimeout => LlmError::TransportTimeout { message },
                K::FileUploadOutcomeUnknown => LlmError::FileUploadOutcomeUnknown { message },
                K::TlsCert => LlmError::tls_cert(message),
                K::StreamInterrupted => LlmError::StreamInterrupted { message },
                K::CostUnavailable => LlmError::CostUnavailable { message },
                K::UnsupportedCapability => LlmError::UnsupportedCapability {
                    capability: message,
                },
            }
        }
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
        E::ProviderTimeout { message, status } => LlmError::ProviderTimeout { message, status },
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
) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
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
        ExecutionUsage {
            report: report.clone(),
            inference: inference.clone(),
            context_tokens: Some(counts.total()),
            provider_reported_total_tokens: Some(counts.total()),
            provider_metadata: metadata,
            cost_estimate: None,
        },
        completeness,
    ))
}

#[cfg(test)]
#[path = "codec_fixtures.rs"]
pub(crate) mod codec_fixtures;

#[cfg(test)]
mod upgrade_tests {
    use super::codec_fixtures::{Codec, FixtureCodec, FixtureInput, HistoryFixture};
    use super::*;
    use lingxi_llm_client::providers::anthropic::{
        native::AnthropicHostedTool,
        types::{
            AnthropicCodeExecutionConfig, AnthropicSkillRef, AnthropicSkillScope,
            AnthropicWebFetchConfig,
        },
    };

    fn projected_history(native: wire::ContentBlock) -> Vec<ContentBlock> {
        let decoded: wire::ChatResponse = serde_json::from_value(json!({
            "message": {"role": "assistant", "content": [native]},
            "model": "m", "stop_reason": "end_turn", "usage": wire::UsageReport::default()
        }))
        .unwrap();
        project_response(
            decoded,
            ProviderResponse::json(200, json!({"content": []})),
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap()
        .content
    }

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
        req.input.native_options.push(extension.clone());
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
        let mut req = HistoryFixture::new("claude-sonnet-4-6");
        req.tools.push(ToolDeclaration {
            name: "lookup".into(),
            input_schema: json!({"type":"object"}),
            extra: serde_json::from_value(json!({"allowed_callers":["direct"]})).unwrap(),
            ..Default::default()
        });
        let projected = req
            .canonical(wire::ProtocolFamily::AnthropicMessages)
            .unwrap()
            .input;
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
        req.input.max_tokens = Some(1024);
        req.input.hosted_tools = vec![
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
        req.input.max_tokens = Some(1024);
        req.input.hosted_tools.push(
            AnthropicHostedTool::CodeExecution(AnthropicCodeExecutionConfig {
                skills: vec![AnthropicSkillRef::custom("skill_example", scope)],
                ..Default::default()
            })
            .into(),
        );
        req.execution.account_scope = Some("workspace-b".into());
        assert!(anthropic_codec().encode_request(&req).is_err());
        req.execution.account_scope = Some("workspace-a".into());
        assert!(anthropic_codec().encode_request(&req).is_ok());
        let roundtrip: LlmRequest =
            serde_json::from_value(serde_json::to_value(req).unwrap()).unwrap();
        assert!(roundtrip.execution.account_scope.is_none());
    }

    #[test]
    fn legacy_system_cache_scope_and_ttl_survive_typed_cache_projection() {
        let mut req = HistoryFixture::new("claude-sonnet-4-6").with_user_text("Hello");
        req.request.input.max_tokens = Some(1024);
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
        let native = wire::ContentBlock::ToolUse { input_json: None,
            id: wire::ToolUseId::new("tool-1"),
            name: "lookup".into(),
            input: json!({"key":"value"}),
            provider_id: None,
            caller: Some(json!({"type":"code_execution_20260120","tool_id":"server-1"})),
            toolset_name: None,
            thought_signature: None,
        };
        let message = Message { api_output_config: None,
            role: "assistant".into(),
            content: projected_history(native.clone()),
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
            HistoryEvent::ContentBlockStart {
                content_block: ContentBlock::ToolCall { .. },
                ..
            }
        )));
        let usage = events
            .iter()
            .find_map(|event| match event {
                HistoryEvent::MessageDelta { usage, .. } => usage.as_ref(),
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
        let projected = projected_history(wire::ContentBlock::ProviderContent {
            protocol: wire::ProtocolFamily::AnthropicMessages,
            value: value.clone(),
        });
        assert!(matches!(
            &projected[0],
            ContentBlock::ProviderContent { .. }
        ));
        let restored = message_content(
            &Message { api_output_config: None,
                role: "assistant".into(),
                content: projected,
            },
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(
            restored,
            vec![wire::ContentBlock::ProviderContent {
                protocol: wire::ProtocolFamily::AnthropicMessages,
                value,
            }]
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
                    .any(|event| matches!(event, HistoryEvent::ContentBlockStop { .. })),
                "{events:?}"
            );
        }
        let events = decoder
            .events(vec![Ok(wire::StreamEvent::BlockEnd { block: 0 })])
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HistoryEvent::ContentBlockStop { index: 0 }))
        );
        let companion = events
            .iter()
            .find_map(|event| match event {
                HistoryEvent::ContentBlockStart { content_block, .. } => {
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
                HistoryEvent::ContentBlockStart { content_block, .. } if replay_companion(content_block).is_some())));
            let closed = decoder
                .events(vec![Ok(wire::StreamEvent::BlockEnd { block: 0 })])
                .unwrap();
            let replay = closed.iter().find_map(|event| match event {
                HistoryEvent::ContentBlockStart { content_block, .. } => {
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
        let native = wire::ContentBlock::ToolUse { input_json: None,
            id: wire::ToolUseId::new("call-1"),
            name: "click".into(),
            input: json!({}),
            provider_id: None,
            caller: None,
            toolset_name: Some("browser".into()),
            thought_signature: None,
        };
        let mut req = HistoryFixture::new("claude-sonnet-4-6");
        req.messages.push(Message { api_output_config: None,
            role: "assistant".into(),
            content: projected_history(native.clone()),
        });
        req.messages.push(Message { api_output_config: None,
            role: "user".into(),
            content: vec![ContentBlock::ToolResult { output_projection: None,
                tool_call_id: "call-1".into(),
                output: json!("clicked"),
                is_error: Some(false),
                cache_control: None,
                cache_reference: None,
            }],
        });
        let projected = req
            .canonical(wire::ProtocolFamily::AnthropicMessages)
            .unwrap()
            .input;
        assert!(
            matches!(&projected.messages[1].content[0], wire::ContentBlock::ToolResult { toolset_name: Some(name), .. } if name == "browser")
        );
    }

    #[test]
    fn history_cache_breakpoints_follow_filtered_message_and_block_positions() {
        let native = wire::ContentBlock::ToolUse { input_json: None,
            id: wire::ToolUseId::new("call-1"),
            name: "lookup".into(),
            input: json!({}),
            provider_id: None,
            caller: Some(json!({"type":"direct"})),
            toolset_name: None,
            thought_signature: None,
        };
        let mut req = HistoryFixture::new("claude-sonnet-4-6");
        req.messages.push(Message { api_output_config: None,
            role: "assistant".into(),
            content: vec![],
        });
        let mut history = projected_history(native);
        assert_eq!(
            history.len(),
            2,
            "display block plus native replay companion"
        );
        history.push(ContentBlock::Text {
            text: "retained".into(),
            cache_control: Some(CacheControl::Ephemeral),
            citations: None,
        });
        req.messages.push(Message { api_output_config: None,
            role: "assistant".into(),
            content: history,
        });
        let projected = req
            .canonical(wire::ProtocolFamily::AnthropicMessages)
            .unwrap()
            .input;
        assert_eq!(projected.messages.len(), 1);
        assert_eq!(projected.messages[0].content.len(), 2);
        assert_eq!(
            projected.prompt_cache.breakpoints[0].position,
            wire::CachePosition::Message { index: 0, block: 1 }
        );
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
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HistoryEvent::MessageDelta { usage: None, .. }))
        );
        let observation = events
            .iter()
            .find_map(|event| match event {
                HistoryEvent::ContentBlockStart {
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
            &Message { api_output_config: None,
                role: "assistant".into(),
                content: vec![observation],
            },
            wire::ProtocolFamily::OpenAiResponses,
        )
        .unwrap();
        assert!(replay.is_empty());
    }

    #[test]
    fn host_labels_preserve_wire_facts_without_guessing_duplicate_rows() {
        let mut host = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "anthropic")
            .unwrap();
        host.models.truncate(1);
        let mut source = host.wire_profile.clone().unwrap();
        source
            .models
            .retain(|model| model.request_model == host.models[0].request_model);
        assert_eq!(source.models.len(), 1);
        source.models[0].info.features.effort.default = Some(wire::ReasoningEffort::Low);
        host.models[0].display_model = "custom label".into();
        host.wire_profile = Some(source.clone());
        assert_eq!(
            profile(&host).unwrap().models[0]
                .info
                .features
                .effort
                .default,
            Some(wire::ReasoningEffort::Low)
        );
        let mut duplicate = source.models[0].clone();
        duplicate.display_model = "second row".into();
        duplicate.info.features.effort.default = Some(wire::ReasoningEffort::High);
        source.models.push(duplicate);
        host.wire_profile = Some(source);
        assert_eq!(
            profile(&host).unwrap().models[0]
                .info
                .features
                .effort
                .default,
            None
        );
        host.models[0].display_model = "second row".into();
        assert_eq!(
            profile(&host).unwrap().models[0]
                .info
                .features
                .effort
                .default,
            Some(wire::ReasoningEffort::High)
        );
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

#[cfg(test)]
mod route_adaptation_tests {
    use super::*;
    #[test]
    fn dropping_foreign_blocks_remaps_exact_strings_without_changing_source() {
        let mut request = LlmRequest::new("model");
        request.input.messages.push(wire::ConversationMessage {
            role: wire::MessageRole::Assistant,
            native_options: vec![],
            content: vec![
                wire::ContentBlock::Thinking {
                    text: "foreign".into(),
                    signature: Some("gemini-signature".into()),
                },
                wire::ContentBlock::Text {
                    text: "retained".into(),
                    thought_signature: None,
                    citations: None,
                },
            ],
        });
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/1/text".into(), vec![0xd800]);
        let adapted = adapt_request(
            &request,
            wire::ProtocolFamily::GeminiGenerateContent,
            wire::ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(adapted.input.messages[0].content.len(), 1);
        assert_eq!(
            adapted
                .execution
                .message_json_string_overrides
                .get("/messages/0/content/0/text"),
            Some(&vec![0xd800])
        );
        assert_eq!(request.input.messages[0].content.len(), 2);
        assert!(
            request
                .execution
                .message_json_string_overrides
                .contains_key("/messages/0/content/1/text")
        );
    }
}

#[cfg(test)]
mod fallback_billing_mode_inheritance_tests {
    use super::*;
    use lingxi_llm_client::protocol::{InferenceReport, Submission};
    use lingxi_llm_client::providers::anthropic::fallback_response::{
        UsageIteration, UsageIterations,
    };

    fn fallback_iterations(model: &str) -> UsageIterations {
        UsageIterations {
            served_fallback_model: Some(model.to_string()),
            entries: vec![UsageIteration {
                r#type: "fallback_message".into(),
                model: Some(model.to_string()),
                input_tokens: 1_000.0,
                output_tokens: 1_000.0,
                cache_read_input_tokens: 0.0,
                cache_creation_input_tokens: 0.0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn omitted_mode_inherits_pinned_anthropic_row_and_explicit_unknown_does_not() {
        // This consumes the real bundled profile from the llm-client source
        // the root Cargo.toml selects; it introduces no tariff.
        let mut host = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "anthropic")
            .expect("pinned Anthropic provider profile");
        assert_eq!(host.provider_id, ProviderId::AnthropicFirstParty);
        let model = host
            .models
            .iter()
            .find(|model| model.request_model == "claude-opus-4-6")
            .expect("pinned Anthropic model row")
            .clone();
        let source = host.wire_profile.as_ref().expect("SDK source snapshot");
        let source_row = source
            .models
            .iter()
            .find(|row| {
                row.display_model == model.display_model && row.request_model == model.request_model
            })
            .expect("exact source model row");
        assert_eq!(source.pricing.billing_mode, wire::BillingMode::PerToken);
        assert_eq!(source_row.billing_mode, None);
        let source_rates = source_row.pricing.as_ref().expect("real model rates");
        let expected_total = (1_000.0 * source_rates.input_per_million.unwrap() / 1_000_000.0)
            + (1_000.0 * source_rates.output_per_million.unwrap() / 1_000_000.0);

        host.pricing.billing_mode = None;
        let inherited = profile(&host).expect("project user profile with inherited source mode");
        assert_eq!(inherited.pricing.billing_mode, wire::BillingMode::PerToken);
        let snapshot =
            client::FrozenPricing::capture(&inherited, &model.display_model, &model.request_model)
                .expect("capture exact pinned model row");
        let quote = snapshot
            .estimate_anthropic_server_fallback(
                &fallback_iterations(&model.request_model),
                Some(&model.request_model),
                None,
                &InferenceReport::default(),
                Submission::Interactive,
            )
            .expect("native quote succeeds")
            .expect("served model enters native quote path");
        assert_eq!(
            quote.completeness,
            client::AnthropicFallbackCostCompleteness::Complete
        );
        assert!((quote.total_cost_usd.unwrap() - expected_total).abs() < 1e-12);

        host.pricing.billing_mode = Some(lingxi_core::host::ModelBillingMode::Unknown);
        let explicit_unknown = profile(&host).expect("project explicit unknown override");
        assert_eq!(
            explicit_unknown.pricing.billing_mode,
            wire::BillingMode::Unknown
        );
        let snapshot = client::FrozenPricing::capture(
            &explicit_unknown,
            &model.display_model,
            &model.request_model,
        )
        .expect("capture exact pinned model row");
        let quote = snapshot
            .estimate_anthropic_server_fallback(
                &fallback_iterations(&model.request_model),
                Some(&model.request_model),
                None,
                &InferenceReport::default(),
                Submission::Interactive,
            )
            .expect("native quote succeeds")
            .expect("served model enters native quote path");
        assert_eq!(
            quote.completeness,
            client::AnthropicFallbackCostCompleteness::Incomplete
        );
        assert!(quote.total_cost_usd.is_none());
    }

    #[test]
    fn cloud_claude_wrappers_without_source_rates_stay_incomplete() {
        let model = "claude-opus-4-6";
        let sdk_profiles = client::builtin_providers().expect("pinned SDK catalog");
        let wrappers = [
            (
                "bedrock-claude-user",
                ProviderId::BedrockClaude,
                ProtocolFamily::BedrockClaude,
                "bedrock-claude",
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                AuthStrategy::AwsSigV4,
                Some(SigningConfig {
                    region: "us-east-1".into(),
                    service: "bedrock".into(),
                }),
            ),
            (
                "vertex-claude-user",
                ProviderId::VertexClaude,
                ProtocolFamily::VertexClaude,
                "vertex-claude",
                "https://vertex.example.com",
                AuthStrategy::GcpToken,
                None,
            ),
            (
                "foundry-claude-user",
                ProviderId::FoundryClaude,
                ProtocolFamily::FoundryClaude,
                "foundry-claude",
                "https://foundry.example.com",
                AuthStrategy::ApiKey,
                None,
            ),
        ];

        for (profile_name, provider_id, protocol, sdk_id, base_url, auth, signing) in wrappers {
            assert!(sdk_profiles.iter().all(|source| {
                source.provider_id.as_str() != sdk_id || source.protocol != protocol
            }));
            let host = ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id,
                profile_name: profile_name.into(),
                base_url: base_url.into(),
                protocol,
                auth,
                credential: CredentialConfig::HostManaged {
                    id: profile_name.into(),
                },
                models: vec![ModelProfile {
                    display_model: model.into(),
                    request_model: model.into(),
                    billing_model: model.into(),
                    aliases: Vec::new(),
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities::default(),
                }],
                pricing: PricingConfig::default(),
                signing,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            };
            let projected = profile(&host).expect("project unpriced cloud wrapper");
            assert_eq!(projected.pricing.billing_mode, wire::BillingMode::Unknown);
            assert!(projected.models[0].pricing.is_none());
            let snapshot = client::FrozenPricing::capture(&projected, model, model)
                .expect("capture configured cloud model identity");
            let quote = snapshot
                .estimate_anthropic_server_fallback(
                    &fallback_iterations(model),
                    Some(model),
                    None,
                    &InferenceReport::default(),
                    Submission::Interactive,
                )
                .expect("cloud quote path succeeds")
                .expect("served model enters native quote path");
            assert_eq!(
                quote.completeness,
                client::AnthropicFallbackCostCompleteness::Incomplete,
                "{profile_name} has no exact SDK row or tariff"
            );
            assert!(quote.total_cost_usd.is_none());
        }
    }
}
