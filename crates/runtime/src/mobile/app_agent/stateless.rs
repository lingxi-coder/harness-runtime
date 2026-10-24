//! Independent stateless Local App understanding requests.
use super::{schema::NativeSchema, AppAgentFactory};
use crate::app_agent_input;
use cost::{
    CostCalculator, ModelRef as CostModelRef, PricingCatalog, ProviderId as CostProviderId,
};
use lingxi_core::{
    host::json_schema_validator::{CompiledJsonSchema, JsonSchemaValidation},
    types::{utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId},
};
use local_app::v2::agent::{Admission, Input, TaskResult};
use serde_json::{json, Value};
use sidequery::{
    ProviderSideQueryClient, QuerySource, SideQueryClient, SideQueryRequest,
    StrictStructuredQueryRequest,
};

impl AppAgentFactory {
    pub(super) async fn query_stateless(
        &self,
        admission: &Admission,
        input: &Input,
    ) -> Result<(TaskResult, Value), String> {
        if !admission.start || !admission.tools.is_empty() || !admission.skills.is_empty() {
            return Err("invalid_admission".into());
        }
        let listing = self
            .api
            .model_listings()
            .into_iter()
            .find(|listing| {
                !listing.hidden
                    && listing.profile_name == admission.task.model.provider_id
                    && listing.request_model == admission.task.model.model_id
            })
            .ok_or("model_unavailable")?;
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
        if prepared.uses_vision && !listing.capabilities.vision {
            return Err("model_vision_unsupported".into());
        }
        let schema = input
            .output_schema
            .as_ref()
            .map(NativeSchema::compile)
            .transpose()?;
        if schema.is_some() && !listing.capabilities.tools {
            return Err("model_tools_unsupported".into());
        }
        let message = user_message(prepared.blocks);
        let query_source = QuerySource::Custom("local_app_agent".into());
        let client = ProviderSideQueryClient::from_service(self.api.clone());
        let (result, usage) = if let Some(schema) = schema {
            let response = client
                .query_json_schema(StrictStructuredQueryRequest {
                    model_attempt: None,
                    model: admission.task.model.model_id.clone(),
                    profile: Some(admission.task.model.provider_id.clone()),
                    system_prompt: None,
                    messages: vec![message],
                    schema: input.output_schema.clone().expect("compiled schema source"),
                    max_tokens: Some(8192),
                    temperature: None,
                    query_source,
                    skip_system_prompt_prefix: true,
                })
                .await
                .map_err(map_side_query_error)?;
            let projection = Utf16JsonProjection::from(
                serde_json::to_string(&response.value).map_err(|_| "invalid_output")?,
            );
            if !matches!(
                schema.validate(&projection).await,
                Ok(JsonSchemaValidation::Valid)
            ) {
                return Err("invalid_output".into());
            }
            (
                TaskResult {
                    text: String::new(),
                    output: Some(response.value),
                    attachments: Vec::new(),
                    chat_id: None,
                    message_id: None,
                },
                response.usage,
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
                    query_source,
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
        let cost_nano = estimate_usage_cost(&self.pricing, &listing, &usage);
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
        sidequery::SideQueryError::StructuredOutputUnsupported => "model_tools_unsupported".into(),
        _ => "model_api_failed".into(),
    }
}

fn estimate_usage_cost(
    catalog: &cost::PricingCatalog,
    listing: &llm_runtime::ModelListing,
    usage: &cost::Usage,
) -> u64 {
    let model = CostModelRef {
        provider: cost_provider(&listing.provider_id),
        model: listing.billing_model.clone(),
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
