use super::*;
use crate::purposes::QuerySource;
use crate::side_query::StrictStructuredQueryRequest;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use llm_runtime::computer::{scope_computer_request, ComputerRequestProjection};
use llm_runtime::services::sdk as wire_sdk;
use serde_json::json;
use std::sync::Mutex;

#[derive(Default)]
struct Capture {
    requests: Mutex<Vec<serde_json::Value>>,
    summary: bool,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: wire_sdk::HttpRequest,
    ) -> Result<wire_sdk::StreamResponse, wire_sdk::protocol::LlmError> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap());
        if self.summary {
            use futures_util::StreamExt;
            let body = serde_json::to_vec(&json!({"id":"summary-response","model":"model",
                "content":[{"type":"text","text":"summary"}],"stop_reason":"end_turn",
                "usage":{"input_tokens":2,"output_tokens":1}}))
            .unwrap();
            return Ok(wire_sdk::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures_util::stream::once(async move { Ok(body.into()) }).boxed(),
            });
        }
        Err(wire_sdk::protocol::LlmError::InvalidRequest {
            message: "fixture stopped after capture".into(),
        })
    }
}

fn user(text: &str) -> ConversationMessage {
    ConversationMessage::User {
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: text.into(),
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    }
}

#[tokio::test]
async fn direct_plain_and_strict_queries_ignore_the_parent_computer_continuation() {
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"open_ai","profile_name":"test","base_url":"https://api.openai.com/v1","protocol":"open_ai_responses",
        "auth":"api_key","credential":{"type":"static","id":"fixture"},
        "models":[{"display_model":"model","request_model":"model","billing_model":"model",
        "capabilities":{"streaming":true,"tools":true,"vision":true,"documents":false,"reasoning":false,"structured_output":true}}]
    }]})).unwrap();
    let transport = Arc::new(Capture::default());
    let client = ProviderSideQueryClient {
        backend: ProviderSideQueryBackend::Direct {
            client: ModelRuntime::from_config(config)
                .unwrap()
                .with_credential_provider(Arc::new(StaticCredentialProvider::new(
                    Credential::ApiKey("fixture-key".into()),
                ))),
            transport: transport.clone(),
        },
    };
    let continuation = serde_json::from_value(json!({
        "protocol":"open_ai_responses","response_id":"parent-response","provider_id":"open_ai",
        "profile_name":"test","endpoint_fingerprint":"endpoint","account_scope":"account","request_model":"model"
    })).unwrap();
    scope_computer_request(
        Some(ComputerRequestProjection {
            native: None,
            continuation: Some(continuation),
            binding: None,
            submission: None,
        }),
        async {
            let messages = vec![user("early context"), user("summarize all context")];
            let plain = client
                .query(SideQueryRequest {
                    model_attempt: None,
                    model: "model".into(),
                    profile: Some("test".into()),
                    system_prompt: None,
                    messages: messages.clone(),
                    tools: vec![],
                    tool_choice: None,
                    output_format: None,
                    max_tokens: 512,
                    max_retries: 0,
                    temperature: None,
                    thinking: None,
                    effort: None,
                    stop_sequences: vec![],
                    query_source: QuerySource::Compaction,
                    skip_system_prompt_prefix: true,
                })
                .await;
            let strict = client
                .query_json_schema(StrictStructuredQueryRequest {
                    model_attempt: None,
                    model: "model".into(),
                    profile: Some("test".into()),
                    system_prompt: None,
                    messages,
                    schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
                    max_tokens: 512,
                    temperature: None,
                    query_source: QuerySource::Compaction,
                    skip_system_prompt_prefix: true,
                })
                .await;
            let requests = transport.requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                2,
                "both independent histories must reach dispatch: plain={plain:?}, strict={strict:?}"
            );
            for request in requests.iter() {
                assert!(request.get("previous_response_id").is_none());
                assert!(request.to_string().contains("early context"));
                assert!(request.to_string().contains("summarize all context"));
            }
        },
    )
    .await;
}

#[tokio::test]
async fn session_compaction_fork_and_estimation_keep_their_own_function_catalog() {
    use crate::{CacheSafeParams, ForkedAgentRequest, ForkedAgentRunner};
    use llm_runtime::computer::ComputerNativeDeclaration;
    use wire_sdk::protocol::computer::{
        ComputerCapabilities, ComputerFrame, ComputerOperationKind, NativeComputerProvider,
    };
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"test","base_url":"https://api.anthropic.com","protocol":"anthropic_messages",
        "auth":"api_key","credential":{"type":"static","id":"fixture"},
        "models":[{"display_model":"model","request_model":"model","billing_model":"model",
        "capabilities":{"streaming":true,"tools":true,"vision":true,"documents":false,"reasoning":false,"structured_output":false}}]
    }]})).unwrap();
    let transport = Arc::new(Capture {
        summary: true,
        ..Default::default()
    });
    let service = Arc::new(llm_runtime::ApiService::new(
        Arc::new(
            ModelRuntime::from_config(config)
                .unwrap()
                .with_credential_provider(Arc::new(StaticCredentialProvider::new(
                    Credential::ApiKey("fixture-key".into()),
                ))),
        ),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    ));
    let client = Arc::new(ProviderSideQueryClient::from_service(service));
    let tools = vec![
        json!({"name":"computer","description":"Manage desktop","input_schema":{"type":"object"}}),
    ];
    let prefix = vec![user("early context")];
    let runner = ForkedAgentRunner::new().with_side_query_client(client.clone(), "model".into());
    scope_computer_request(
        Some(ComputerRequestProjection {
            native: Some(ComputerNativeDeclaration {
                provider: NativeComputerProvider::Anthropic,
                capabilities: ComputerCapabilities {
                    operations: vec![ComputerOperationKind::Screenshot],
                },
                frame: ComputerFrame {
                    width: 800,
                    height: 600,
                    geometry_version: "parent-frame".into(),
                },
            }),
            continuation: None,
            binding: None,
            submission: None,
        }),
        async {
            let estimate = client
                .estimate_request(CanonicalSideQueryRequest::Plain(SideQueryRequest {
                    model_attempt: None,
                    model: "model".into(),
                    profile: Some("test".into()),
                    system_prompt: None,
                    messages: prefix.clone(),
                    tools: tools.clone(),
                    tool_choice: None,
                    output_format: None,
                    max_tokens: 512,
                    max_retries: 0,
                    temperature: None,
                    thinking: None,
                    effort: None,
                    stop_sequences: vec![],
                    query_source: QuerySource::Compaction,
                    skip_system_prompt_prefix: true,
                }))
                .unwrap();
            assert!(estimate.serialized_bytes > 0);
            let result = runner
                .run(ForkedAgentRequest {
                    prompt_messages: vec![user("summarize all context")],
                    cache_safe_params: CacheSafeParams {
                        system_prompt: Arc::from("Summarize"),
                        user_context: Default::default(),
                        system_context: Default::default(),
                        user_context_message: None,
                        tool_use_options: tool_api::ToolUseOptions {
                            debug: false,
                            verbose: false,
                            main_loop_model: "model".into(),
                            model_profile: Some("test".into()),
                            max_budget_nano_usd: None,
                            mcp_clients: vec![],
                            is_non_interactive_session: true,
                            custom_system_prompt: None,
                            append_system_prompt: None,
                        },
                        tools: tools.clone(),
                        effort: None,
                        fork_context_messages: prefix.clone(),
                        transcript_path: None,
                        generation: 1,
                    },
                    fork_label: "compaction".into(),
                    query_source: QuerySource::Compaction,
                    max_output_tokens: Some(512),
                })
                .await
                .unwrap();
            assert_eq!(result.final_text, "summary");
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["tools"][0]["name"], "computer");
            assert!(requests[0]["tools"][0]["input_schema"].is_object());
            assert!(requests[0].to_string().contains("early context"));
            assert!(requests[0].to_string().contains("summarize all context"));
        },
    )
    .await;
}
