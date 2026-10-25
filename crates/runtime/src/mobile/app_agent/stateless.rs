//! Independent stateless Local App understanding requests.
use super::{AppAgentFactory, STRUCTURED_OUTPUT_TOOL_NAME, schema::NativeSchema};
use crate::app_agent_input;
use cost::{
    CostCalculator, ModelRef as CostModelRef, PricingCatalog, ProviderId as CostProviderId,
};
use lingxi_core::{
    host::json_schema_validator::{CompiledJsonSchema, JsonSchemaValidation},
    types::{ContentBlock, ConversationMessage, MessageId, utf16_json::Utf16JsonProjection},
};
use local_app::v2::agent::{Admission, Input, TaskResult};
use serde_json::{Value, json};
use sidequery::{
    ProviderSideQueryClient, QuerySource, SideQueryClient, SideQueryRequest,
    StrictStructuredQueryRequest,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AppAgentModelCapabilities {
    pub supports_images: bool,
    pub supports_tools: bool,
    pub supports_native_structured_output: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StructuredOutputMode {
    NativeJsonSchema,
    InternalTool,
}

pub(super) fn app_agent_capabilities(
    api: &llm_runtime::ApiService,
    route: &llm_runtime::MediaRoute,
) -> AppAgentModelCapabilities {
    let can_encode_native_schema = api
        .protocol_for_model(&route.main.request_model, Some(&route.main.profile_name))
        .is_ok_and(|protocol| protocol.encodes_response_format());
    route_capabilities(route, can_encode_native_schema)
}

fn route_capabilities(
    route: &llm_runtime::MediaRoute,
    can_encode_native_schema: bool,
) -> AppAgentModelCapabilities {
    AppAgentModelCapabilities {
        // This task's authorization belongs to the selected model route. A
        // general vision delegate would silently switch models.
        supports_images: route.main.capabilities.vision,
        supports_tools: route.main.capabilities.tools,
        supports_native_structured_output: route.main.capabilities.structured_output
            && can_encode_native_schema,
    }
}

pub(super) fn supports_structured_output(capabilities: AppAgentModelCapabilities) -> bool {
    capabilities.supports_native_structured_output || capabilities.supports_tools
}

pub(super) fn harness_structured_output_available(capabilities: AppAgentModelCapabilities) -> bool {
    capabilities.supports_tools
}

fn structured_output_mode(
    capabilities: AppAgentModelCapabilities,
) -> Result<StructuredOutputMode, &'static str> {
    // Prefer the trusted, host-owned result tool whenever the selected route
    // can call tools. Some providers advertise native structured output but
    // reject the strict response_format shape for a particular resolved route;
    // the forced tool path keeps schema validation on the host and uses the
    // same model/profile without invoking that separate codec path.
    if capabilities.supports_tools {
        Ok(StructuredOutputMode::InternalTool)
    } else if capabilities.supports_native_structured_output {
        Ok(StructuredOutputMode::NativeJsonSchema)
    } else {
        Err("model_structured_output_unsupported")
    }
}

pub(super) fn resolve_selected_route(
    api: &llm_runtime::ApiService,
    model: &str,
    provider: &str,
) -> Result<llm_runtime::MediaRoute, &'static str> {
    resolve_selected_route_with(model, provider, |model, profile| {
        api.resolve_media_route(model, profile)
    })
}

fn resolve_selected_route_with(
    model: &str,
    provider: &str,
    resolve: impl FnOnce(&str, Option<&str>) -> Result<llm_runtime::MediaRoute, llm_runtime::LlmError>,
) -> Result<llm_runtime::MediaRoute, &'static str> {
    resolve(model, Some(provider)).map_err(|_| "model_unavailable")
}

fn structured_output_tool(schema: &Value) -> Result<llm_runtime::ToolDeclaration, &'static str> {
    let tool_schema = structured_tool_schema(schema);
    let description = lingxi_llm_client::protocol::JsonPayload::from_generated_value(&json!(
        "Return the requested result in the result property, matching its schema exactly."
    ))
    .map_err(|_| "invalid_output_schema")?;
    let input_schema = lingxi_llm_client::protocol::JsonPayload::from_generated_value(&tool_schema)
        .map_err(|_| "invalid_output_schema")?;
    Ok(llm_runtime::ToolDeclaration {
        name: STRUCTURED_OUTPUT_TOOL_NAME.into(),
        description,
        input_schema,
        ..Default::default()
    })
}

fn structured_tool_schema(schema: &Value) -> Value {
    let mut result_schema = schema.clone();
    if let Some(object) = result_schema.as_object_mut() {
        if !object.contains_key("$id") {
            // The nested schema becomes its own Draft-7 resource so local
            // references keep their original root after the result wrapper is
            // added. Draft-7 ignores the legacy `id` spelling. This identifier
            // is private to the function declaration.
            object.insert(
                "$id".into(),
                Value::String("urn:lingxi:local-app-agent-result".into()),
            );
        }
    }
    let tool_schema = json!({
        "type": "object",
        "properties": {"result": result_schema},
        "required": ["result"],
        "additionalProperties": false
    });
    tool_schema
}

fn structured_tool_result(calls: &[Utf16JsonProjection]) -> Result<Value, &'static str> {
    if calls.is_empty() {
        return Err("structured_output_missing");
    }
    if calls.len() != 1 {
        return Err("invalid_output");
    }
    let serialized = calls[0].to_json_string().map_err(|_| "invalid_output")?;
    let call: Value = serde_json::from_str(&serialized).map_err(|_| "invalid_output")?;
    if call.get("name").and_then(Value::as_str) != Some(STRUCTURED_OUTPUT_TOOL_NAME) {
        return Err("invalid_output");
    }
    let input = call
        .get("input")
        .and_then(Value::as_object)
        .ok_or("invalid_output")?;
    let result = input
        .get("result")
        .cloned()
        .ok_or("structured_output_missing")?;
    if input.len() != 1 {
        return Err("invalid_output");
    }
    if result.is_null() {
        return Err("structured_output_not_representable");
    }
    Ok(result)
}

fn internal_structured_query_request(
    model: &str,
    profile: &str,
    message: ConversationMessage,
    tool: llm_runtime::ToolDeclaration,
) -> SideQueryRequest {
    SideQueryRequest {
        enable_prompt_caching: false,
        model_attempt: None,
        api_attempts_left: None,
        native_prompt_context: None,
        model: model.to_owned(),
        model_source: None,
        profile: Some(profile.to_owned()),
        system_prompt: None,
        typed_attachment_ids: Default::default(),
        task_deliveries: Default::default(),
        system_prompt_input: None,
        messages: vec![message],
        tools: vec![tool],
        tool_choice: Some(json!({
            "type": "tool",
            "name": STRUCTURED_OUTPUT_TOOL_NAME
        })),
        output_format: None,
        max_tokens: Some(8192),
        max_retries: 0,
        temperature: None,
        thinking: Some(llm_runtime::model::thinking::ThinkingConfig::Disabled),
        effort: None,
        stop_sequences: Vec::new(),
        query_source: QuerySource::Custom("local_app_agent".into()),
        skip_system_prompt_prefix: true,
    }
}

async fn query_internal_structured_output(
    client: &dyn SideQueryClient,
    model: &str,
    profile: &str,
    message: ConversationMessage,
    schema: &Value,
) -> Result<(Value, cost::Usage), String> {
    let tool = structured_output_tool(schema).map_err(str::to_owned)?;
    let response = client
        .query(internal_structured_query_request(
            model, profile, message, tool,
        ))
        .await
        .map_err(map_side_query_error)?;
    let result = structured_tool_result(&response.tool_calls).map_err(str::to_owned)?;
    Ok((result, response.usage))
}

async fn validate_schema_result(
    value: Value,
    schema: &NativeSchema,
) -> Result<Value, &'static str> {
    if value.is_null() {
        return Err("structured_output_not_representable");
    }
    let projection = Utf16JsonProjection::from_serde_value(&value).map_err(|_| "invalid_output")?;
    if !matches!(
        schema.validate(&projection).await,
        Ok(JsonSchemaValidation::Valid)
    ) {
        return Err("invalid_output");
    }
    Ok(value)
}

impl AppAgentFactory {
    pub(super) async fn query_stateless(
        &self,
        admission: &Admission,
        input: &Input,
    ) -> Result<(TaskResult, Value), String> {
        if !admission.start || !admission.tools.is_empty() || !admission.skills.is_empty() {
            return Err("invalid_admission".into());
        }
        let route = resolve_selected_route(
            &self.api,
            &admission.task.model.model_id,
            &admission.task.model.provider_id,
        )?;
        let capabilities = app_agent_capabilities(&self.api, &route);
        if !self
            .api
            .credential_source(
                &admission.task.model.model_id,
                Some(&admission.task.model.provider_id),
            )
            .await
            .is_ok_and(|source| {
                !matches!(
                    source,
                    llm_runtime::CredentialSource::None | llm_runtime::CredentialSource::Unknown
                )
            })
        {
            return Err("model_credentials_unavailable".into());
        }
        let prepared = app_agent_input::prepare(input).await?;
        if prepared.uses_vision && !capabilities.supports_images {
            return Err("model_vision_unsupported".into());
        }
        let schema = input
            .output_schema
            .as_ref()
            .map(NativeSchema::compile)
            .transpose()?;
        let output_mode = schema
            .as_ref()
            .map(|_| structured_output_mode(capabilities))
            .transpose()?;
        let message = user_message(prepared.blocks);
        let client = ProviderSideQueryClient::from_service(self.api.clone());
        let (result, usage) = if let Some(schema) = schema {
            let schema_source = input.output_schema.clone().expect("compiled schema source");
            let (value, usage) = match output_mode.expect("schema mode was resolved") {
                StructuredOutputMode::NativeJsonSchema => {
                    let response = client
                        .query_json_schema(StrictStructuredQueryRequest {
                            model_attempt: None,
                            model: admission.task.model.model_id.clone(),
                            profile: Some(admission.task.model.provider_id.clone()),
                            system_prompt: None,
                            messages: vec![message],
                            schema: schema_source,
                            max_tokens: Some(8192),
                            temperature: None,
                            query_source: QuerySource::Custom("local_app_agent".into()),
                            skip_system_prompt_prefix: true,
                        })
                        .await
                        .map_err(map_side_query_error)?;
                    (response.value, response.usage)
                }
                StructuredOutputMode::InternalTool => {
                    query_internal_structured_output(
                        &client,
                        &admission.task.model.model_id,
                        &admission.task.model.provider_id,
                        message,
                        &schema_source,
                    )
                    .await?
                }
            };
            let value = validate_schema_result(value, &schema).await?;
            (
                TaskResult {
                    text: String::new(),
                    output: Some(value),
                    attachments: Vec::new(),
                    chat_id: None,
                    message_id: None,
                },
                usage,
            )
        } else {
            let response = client
                .query(SideQueryRequest {
                    enable_prompt_caching: false,
                    model_attempt: None,
                    api_attempts_left: None,
                    native_prompt_context: None,
                    model: admission.task.model.model_id.clone(),
                    model_source: None,
                    profile: Some(admission.task.model.provider_id.clone()),
                    system_prompt: None,
                    typed_attachment_ids: Default::default(),
                    task_deliveries: Default::default(),
                    system_prompt_input: None,
                    messages: vec![message],
                    tools: Vec::new(),
                    tool_choice: None,
                    output_format: None,
                    max_tokens: Some(8192),
                    max_retries: 0,
                    temperature: None,
                    thinking: Some(llm_runtime::model::thinking::ThinkingConfig::Disabled),
                    effort: None,
                    stop_sequences: Vec::new(),
                    query_source: QuerySource::Custom("local_app_agent".into()),
                    skip_system_prompt_prefix: true,
                })
                .await
                .map_err(map_side_query_error)?;
            if !response.tool_calls.is_empty() {
                return Err("model_tools_unsupported".into());
            }
            let text = response
                .text
                .and_then(|text| text.string_at("").map(str::to_owned))
                .ok_or("model_response_empty")?;
            (
                TaskResult {
                    text,
                    output: None,
                    attachments: Vec::new(),
                    chat_id: None,
                    message_id: None,
                },
                response.usage,
            )
        };
        let cost_nano = estimate_usage_cost(
            &self.pricing,
            &route.main.provider_id,
            &route.main.pricing_model.billing_model,
            &usage,
        );
        let usage = json!({
            "inputTokens": usage.tokens.input,
            "outputTokens": usage.tokens.output,
            "costUsd": cost_nano as f64 / 1_000_000_000.0,
            // SideQuery returns provider usage; host pricing is a catalog estimate.
            "costEstimated": true,
            "turns": 1
        });
        Ok((result, usage))
    }
}

fn user_message(mut blocks: Vec<ContentBlock>) -> ConversationMessage {
    let mut message = ConversationMessage::user(MessageId::new(), String::new());
    if let ConversationMessage::User { content, .. } = &mut message {
        *content = std::mem::take(&mut blocks);
    }
    message
}

fn map_side_query_error(error: sidequery::SideQueryError) -> String {
    match error {
        sidequery::SideQueryError::StructuredOutputUnsupported => {
            "model_structured_output_unsupported".into()
        }
        _ => "model_api_failed".into(),
    }
}

fn estimate_usage_cost(
    catalog: &cost::PricingCatalog,
    provider_id: &llm_runtime::ProviderId,
    billing_model: &str,
    usage: &cost::Usage,
) -> u64 {
    let model = CostModelRef {
        provider: cost_provider(provider_id),
        model: billing_model.to_owned(),
    };
    let pricing = catalog
        .resolve(&model)
        .map(|(pricing, _)| pricing)
        .unwrap_or_else(|_| PricingCatalog::default_unknown_pricing(&model));
    CostCalculator::calculate_nano_usd(usage, &pricing)
}

fn cost_provider(provider: &llm_runtime::ProviderId) -> CostProviderId {
    use llm_runtime::ProviderId as LlmProvider;
    match provider {
        LlmProvider::AnthropicFirstParty
        | LlmProvider::VertexClaude
        | LlmProvider::FoundryClaude => CostProviderId::Anthropic,
        LlmProvider::OpenAI | LlmProvider::AzureOpenAI => CostProviderId::OpenAI,
        LlmProvider::OpenAICompatible { name } => {
            CostProviderId::OpenAICompatible { name: name.clone() }
        }
        LlmProvider::Gemini | LlmProvider::VertexGemini => CostProviderId::GoogleGemini,
        LlmProvider::BedrockClaude => CostProviderId::AmazonBedrock,
        LlmProvider::Custom { name } => CostProviderId::Custom { name: name.clone() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::{
        AuthStrategy, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig, ModelProfile,
        ModelRegistry, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile, Region,
    };
    use serde_json::json;

    fn profile(
        name: &str,
        protocol: ProtocolFamily,
        connection_id: &str,
        order: u32,
        capabilities: Capabilities,
    ) -> ProviderProfile {
        ProviderProfile {
            wire_profile: None,
            regions: Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            profile_name: name.into(),
            base_url: "https://api.example.invalid/v1".into(),
            protocol,
            auth: AuthStrategy::None,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "DeepSeek Flash".into(),
                request_model: "deepseek-flash".into(),
                billing_model: "deepseek-flash".into(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities,
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: ConnectionSpec {
                group: Some("deepseek".into()),
                connection_id: Some(connection_id.into()),
                order,
                ..Default::default()
            },
        }
    }

    fn routing_registry() -> ModelRegistry {
        ModelRegistry::from_config(ClientConfig {
            providers: vec![profile(
                "deepseek",
                ProtocolFamily::OpenAiChat,
                "chat",
                0,
                Capabilities {
                    vision: true,
                    tools: true,
                    structured_output: false,
                    ..Default::default()
                },
            )],
        })
        .expect("configured routing registry")
    }

    #[test]
    fn explicit_profile_route_uses_separate_native_and_tool_capabilities() {
        let registry = routing_registry();
        let picker_row = registry
            .available_models()
            .into_iter()
            .find(|model| model.profile_name == "deepseek")
            .expect("selected picker row");
        assert!(picker_row.capabilities.tools);
        assert!(!picker_row.capabilities.structured_output);

        let route = resolve_selected_route_with("deepseek-flash", "deepseek", |model, profile| {
            registry.resolve_media_route_in(model, profile)
        })
        .expect("effective route resolves");
        assert_eq!(route.main.profile_name, "deepseek");
        let effective = route_capabilities(&route, true);
        assert!(effective.supports_tools);
        assert!(!effective.supports_native_structured_output);
        assert!(supports_structured_output(effective));
        assert_eq!(
            structured_output_mode(effective),
            Ok(StructuredOutputMode::InternalTool)
        );
    }

    struct StructuredToolRecordingClient {
        seen: std::sync::Mutex<Option<SideQueryRequest>>,
    }

    #[async_trait::async_trait]
    impl SideQueryClient for StructuredToolRecordingClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            *self.seen.lock().expect("recorded SideQuery request") = Some(request);
            Ok(sidequery::SideQueryResponse {
                text: None,
                structured: None,
                tool_calls: vec![Utf16JsonProjection::parse(
                    r#"{"id":"call_1","name":"StructuredOutput","input":{"result":{"calories":321}}}"#,
                )
                .expect("tool call is valid JSON")],
                usage: cost::Usage::default(),
                stop_reason: Some("tool_use".into()),
                retry_count: 0,
            })
        }
    }

    #[tokio::test]
    async fn resolved_dual_capability_route_uses_host_forced_tool_not_native_json_schema() {
        let registry = ModelRegistry::from_config(ClientConfig {
            providers: vec![profile(
                "deepseek",
                ProtocolFamily::OpenAiChat,
                "chat",
                0,
                Capabilities {
                    vision: true,
                    tools: true,
                    structured_output: true,
                    ..Default::default()
                },
            )],
        })
        .expect("configured dual-capability route");
        let route = resolve_selected_route_with("deepseek-flash", "deepseek", |model, profile| {
            registry.resolve_media_route_in(model, profile)
        })
        .expect("selected model route resolves");
        assert_eq!(route.main.request_model, "deepseek-flash");
        assert_eq!(route.main.profile_name, "deepseek");
        let can_encode_native_schema = ProtocolFamily::OpenAiChat.encodes_response_format();
        assert!(can_encode_native_schema);
        let capabilities = route_capabilities(&route, can_encode_native_schema);
        assert!(capabilities.supports_tools);
        assert!(capabilities.supports_native_structured_output);
        assert_eq!(
            structured_output_mode(capabilities),
            Ok(StructuredOutputMode::InternalTool),
            "use the same route's tool support before its native response-format path"
        );

        let schema = json!({
            "type":"object",
            "properties":{"calories":{"type":"integer"}},
            "required":["calories"],
            "additionalProperties":false
        });
        let client = StructuredToolRecordingClient {
            seen: std::sync::Mutex::new(None),
        };
        let (value, _) = query_internal_structured_output(
            &client,
            "deepseek-flash",
            "deepseek",
            ConversationMessage::user(MessageId::new(), "Estimate calories.".into()),
            &schema,
        )
        .await
        .expect("forced structured tool returns a result");
        let compiled = NativeSchema::compile(&schema).expect("schema compiles");
        assert_eq!(
            validate_schema_result(value, compiled.as_ref())
                .await
                .expect("tool result passes host schema validation"),
            json!({"calories":321})
        );

        let request = client
            .seen
            .lock()
            .expect("recorded SideQuery request")
            .clone()
            .expect("request was sent");
        assert_eq!(request.model, "deepseek-flash");
        assert_eq!(request.profile.as_deref(), Some("deepseek"));
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, STRUCTURED_OUTPUT_TOOL_NAME);
        assert_eq!(
            request.tool_choice,
            Some(json!({"type":"tool","name":"StructuredOutput"}))
        );
        assert!(request.output_format.is_none());
    }

    #[test]
    fn strict_schema_and_tool_call_capabilities_are_independent() {
        let strict = AppAgentModelCapabilities {
            supports_native_structured_output: true,
            supports_tools: false,
            ..Default::default()
        };
        assert_eq!(
            structured_output_mode(strict),
            Ok(StructuredOutputMode::NativeJsonSchema)
        );
        assert!(supports_structured_output(strict));
        assert!(!strict.supports_tools);
        assert!(!harness_structured_output_available(strict));

        let unsupported = AppAgentModelCapabilities::default();
        assert_eq!(
            structured_output_mode(unsupported),
            Err("model_structured_output_unsupported")
        );
        assert!(!supports_structured_output(unsupported));
        assert!(!harness_structured_output_available(unsupported));
        let tool_only = AppAgentModelCapabilities {
            supports_tools: true,
            ..Default::default()
        };
        assert!(harness_structured_output_available(tool_only));
    }

    #[test]
    fn native_structured_rejection_is_not_reported_as_tool_capability_failure() {
        assert_eq!(
            map_side_query_error(sidequery::SideQueryError::StructuredOutputUnsupported),
            "model_structured_output_unsupported"
        );
    }

    #[test]
    fn native_schema_requires_the_resolved_codec_and_selected_model_vision() {
        let registry = routing_registry();
        let mut route = registry
            .resolve_media_route_in("deepseek-flash", Some("deepseek"))
            .unwrap();
        route.main.capabilities.structured_output = true;
        route.main.capabilities.tools = false;
        route.main.capabilities.vision = false;
        route.vision_delegate = Some(route.main.clone());

        let unsupported_codec = route_capabilities(&route, false);
        assert!(!unsupported_codec.supports_native_structured_output);
        assert!(!supports_structured_output(unsupported_codec));
        assert!(!unsupported_codec.supports_images);

        let supported_codec = route_capabilities(&route, true);
        assert!(supported_codec.supports_native_structured_output);
        assert!(supports_structured_output(supported_codec));
    }

    #[test]
    fn invalid_model_and_foreign_profile_fail_before_side_query() {
        let registry = routing_registry();
        assert_eq!(
            resolve_selected_route_with("missing-model", "deepseek", |model, profile| {
                registry.resolve_media_route_in(model, profile)
            })
            .unwrap_err(),
            "model_unavailable"
        );
        assert_eq!(
            resolve_selected_route_with("deepseek-flash", "other-provider", |model, profile| {
                registry.resolve_media_route_in(model, profile)
            })
            .unwrap_err(),
            "model_unavailable"
        );
    }

    #[tokio::test]
    async fn internal_structured_tool_accepts_wrapped_array_and_scalar_results() {
        let array_schema = NativeSchema::compile(&json!({
            "type":"array",
            "items":{"type":"integer"}
        }))
        .unwrap();
        let array_call = Utf16JsonProjection::parse(
            r#"{"id":"call-1","name":"StructuredOutput","input":{"result":[1,2]}}"#,
        )
        .unwrap();
        let array = structured_tool_result(&[array_call]).unwrap();
        assert_eq!(
            validate_schema_result(array, array_schema.as_ref())
                .await
                .unwrap(),
            json!([1, 2])
        );

        let scalar_schema = NativeSchema::compile(&json!({"type":"string"})).unwrap();
        let scalar_call = Utf16JsonProjection::parse(
            r#"{"id":"call-2","name":"StructuredOutput","input":{"result":"ok"}}"#,
        )
        .unwrap();
        assert_eq!(
            validate_schema_result(
                structured_tool_result(&[scalar_call]).unwrap(),
                scalar_schema.as_ref()
            )
            .await
            .unwrap(),
            json!("ok")
        );
    }

    #[tokio::test]
    async fn internal_result_wrapper_preserves_local_schema_references() {
        let requested_schema = json!({
            "id":"urn:legacy:ignored-by-draft-seven",
            "type":"array",
            "items":{"$ref":"#/definitions/count"},
            "definitions":{"count":{"type":"integer","minimum":0}}
        });
        let wrapped_schema = structured_tool_schema(&requested_schema);
        assert_eq!(
            wrapped_schema["properties"]["result"]["$id"],
            "urn:lingxi:local-app-agent-result"
        );
        let compiled_wrapper = NativeSchema::compile(&wrapped_schema).unwrap();
        assert_eq!(
            validate_schema_result(json!({"result":[1,2]}), compiled_wrapper.as_ref())
                .await
                .unwrap(),
            json!({"result":[1,2]})
        );
        assert_eq!(
            validate_schema_result(json!({"result":[-1]}), compiled_wrapper.as_ref()).await,
            Err("invalid_output")
        );
    }

    #[test]
    fn internal_structured_tool_rejects_missing_extra_and_foreign_calls() {
        assert_eq!(
            structured_tool_result(&[]),
            Err("structured_output_missing")
        );
        let foreign =
            Utf16JsonProjection::parse(r#"{"id":"call-1","name":"Search","input":{"result":{}}}"#)
                .unwrap();
        assert_eq!(structured_tool_result(&[foreign]), Err("invalid_output"));
        let missing =
            Utf16JsonProjection::parse(r#"{"id":"call-1","name":"StructuredOutput","input":{}}"#)
                .unwrap();
        assert_eq!(
            structured_tool_result(&[missing]),
            Err("structured_output_missing")
        );
        let extra = Utf16JsonProjection::parse(
            r#"{"id":"call-1","name":"StructuredOutput","input":{"result":{},"extra":true}}"#,
        )
        .unwrap();
        assert_eq!(structured_tool_result(&[extra]), Err("invalid_output"));
        let first = Utf16JsonProjection::parse(
            r#"{"id":"call-1","name":"StructuredOutput","input":{"result":{}}}"#,
        )
        .unwrap();
        let second = first.clone();
        assert_eq!(
            structured_tool_result(&[first, second]),
            Err("invalid_output")
        );
    }

    #[tokio::test]
    async fn internal_structured_tool_rejects_malformed_and_null_result_values() {
        let schema = NativeSchema::compile(&json!({"type":"object"})).unwrap();
        let malformed = Utf16JsonProjection::parse(
            r#"{"id":"call-1","name":"StructuredOutput","input":{"result":"not-an-object"}}"#,
        )
        .unwrap();
        let result = structured_tool_result(&[malformed]).unwrap();
        assert_eq!(
            validate_schema_result(result, schema.as_ref()).await,
            Err("invalid_output")
        );
        let null = Utf16JsonProjection::parse(
            r#"{"id":"call-1","name":"StructuredOutput","input":{"result":null}}"#,
        )
        .unwrap();
        assert_eq!(
            structured_tool_result(&[null]),
            Err("structured_output_not_representable")
        );
    }
}
