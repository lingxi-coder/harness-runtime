use super::*;
use crate::{QuerySource, StrictStructuredQueryRequest};
use lingxi_core::types::{ConversationMessage, MessageId};

struct NoSend;
#[async_trait]
impl Transport for NoSend {
    async fn send(
        &self,
        _: llm_runtime::services::sdk::HttpRequest,
    ) -> Result<
        llm_runtime::services::sdk::StreamResponse,
        llm_runtime::services::sdk::protocol::LlmError,
    > {
        panic!("budget estimation and preparation must not dispatch");
    }
}

#[tokio::test]
async fn session_schema_estimate_matches_selected_route_generation_and_keeps_canonical_bytes() {
    for supports_schema in [false, true] {
        let mut provider = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .find(|p| p.profile_name == "anthropic")
            .unwrap();
        provider.profile_name = "estimate".into();
        provider.provider_id = ProviderId::Custom {
            name: "estimate".into(),
        };
        provider.base_url = "https://estimate.invalid".into();
        provider.auth = AuthStrategy::None;
        provider.credential = CredentialConfig::None;
        provider.wire_profile = None;
        provider.connection = Default::default();
        provider.models.truncate(1);
        let model = &mut provider.models[0];
        model.display_model = "claude-sonnet-4-6".into();
        model.request_model = "claude-sonnet-4-6".into();
        model.billing_model = "claude-sonnet-4-6".into();
        model.aliases.clear();
        model.capabilities.structured_output = supports_schema;
        let runtime = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![provider],
            })
            .unwrap(),
        );
        let service = Arc::new(ApiService::new(
            runtime.clone(),
            Arc::new(NoSend),
            Default::default(),
            llm_runtime::model::user_agent::UserAgentEnv::default(),
            "test",
            None,
            None,
        ));
        let client = ProviderSideQueryClient::from_service(service.clone());
        let mut estimates = Vec::new();
        let mut bodies = Vec::new();
        for description in [String::new(), "X".repeat(15_000)] {
            let request = StrictStructuredQueryRequest {
                model_attempt: None,
                model: "claude-sonnet-4-6".into(),
                profile: Some("estimate".into()),
                system_prompt: Some("Return JSON.".into()),
                messages: vec![ConversationMessage::user(
                    MessageId::new(),
                    "small input".into(),
                )],
                schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false,"description":description}),
                max_tokens: Some(128),
                temperature: None,
                query_source: QuerySource::FusionAnalyst,
                skip_system_prompt_prefix: true,
            };
            estimates.push(
                client
                    .estimate_request(CanonicalSideQueryRequest::Strict(request.clone()))
                    .unwrap(),
            );
            let system = Utf16JsonProjection::from("Return JSON.");
            let canonical = service
                .build_json_schema_request_with_thinking(
                    &request.model,
                    request.profile.as_deref(),
                    Some(&system),
                    None,
                    request.messages,
                    request.schema,
                    request.max_tokens,
                    None,
                    None,
                    None,
                    Some("fusion_analyst"),
                )
                .unwrap();
            assert_eq!(
                estimates.last().unwrap().input_tokens,
                service.estimate_input_tokens(&canonical).unwrap()
            );
            let prepared = runtime.prepare(&canonical).await.unwrap();
            assert_eq!(
                prepared
                    .provider_request
                    .body_json
                    .payload_at("/output_config/format")
                    .is_some(),
                supports_schema
            );
            bodies.push(prepared.provider_request.body_json);
        }
        assert!(estimates[1].serialized_bytes > estimates[0].serialized_bytes + 14_000);
        if supports_schema {
            assert!(estimates[1].input_tokens > estimates[0].input_tokens + 4_900);
            assert!(!bodies[0].semantically_eq(&bodies[1]).unwrap());
        } else {
            assert_eq!(estimates[0].input_tokens, estimates[1].input_tokens);
            assert!(bodies[0].semantically_eq(&bodies[1]).unwrap());
        }
    }
}
