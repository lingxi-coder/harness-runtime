//! Current native automatic schema gates reach SDK preparation and both drivers.
use async_trait::async_trait;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{
    ApiService, AuthStrategy, Capabilities, ClientConfig, CredentialConfig, LlmRequest,
    ModelProfile, ModelRuntime, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
    SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn own() -> Self {
        let vars = [
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            "CLAUDE_CODE_EXTRA_BODY",
            branding::EFFORT_LEVEL_ENV,
            branding::MODEL_CAPABILITIES_ENV,
            branding::DISABLE_FAST_MODE_ENV,
            branding::DISABLE_STRUCTURED_OUTPUTS_ENV,
            "DISABLE_INTERLEAVED_THINKING",
            "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
            "CLAUDE_CODE_ALWAYS_ENABLE_EFFORT",
        ];
        let result = Self(
            vars.iter()
                .map(|&name| (name, std::env::var_os(name)))
                .collect(),
        );
        for name in vars {
            std::env::remove_var(name);
        }
        result
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn profile(protocol: ProtocolFamily, structured_output: bool) -> ProviderProfile {
    let (name, provider_id, base_url) = match protocol {
        ProtocolFamily::AnthropicMessages => (
            "anthropic",
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        ),
        ProtocolFamily::BedrockClaude => (
            "bedrock",
            ProviderId::BedrockClaude,
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        ),
        ProtocolFamily::VertexClaude => (
            "vertex",
            ProviderId::VertexClaude,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
        ),
        ProtocolFamily::FoundryClaude => (
            "foundry",
            ProviderId::FoundryClaude,
            "https://fixture.services.ai.azure.com/anthropic",
        ),
        ProtocolFamily::OpenAiChat => ("openai", ProviderId::OpenAI, "https://api.openai.com/v1"),
        ProtocolFamily::OpenAiResponses => {
            ("responses", ProviderId::OpenAI, "https://api.openai.com/v1")
        }
        ProtocolFamily::GeminiGenerateContent => (
            "gemini",
            ProviderId::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        _ => panic!("unexpected test route"),
    };
    ProviderProfile {
        wire_profile: None,
        regions: llm_runtime::Region::all(),
        provider_id,
        profile_name: name.into(),
        base_url: base_url.into(),
        protocol,
        auth: AuthStrategy::None,
        credential: CredentialConfig::None,
        models: vec![ModelProfile {
            display_model: "claude-sonnet-4-6".into(),
            request_model: "claude-sonnet-4-6".into(),
            billing_model: "claude-sonnet-4-6".into(),
            aliases: vec![],
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                structured_output,
                ..Default::default()
            },
        }],
        pricing: PricingConfig::default(),
        signing: None,
        azure: None,
        supports_websockets: false,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: None,
        vision_delegate: None,
        connection: Default::default(),
    }
}

fn schema_request() -> LlmRequest {
    let mut request = LlmRequest::new("claude-sonnet-4-6").with_user_text("Return the result");
    request.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonSchema {
        name: "response".into(),
        schema: json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}),
        strict: true,
    };
    request
}

#[derive(Default)]
struct Capture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.0.lock().unwrap().push(request);
        Ok(lingxi_llm_client::StreamResponse {
            status: 400,
            headers: vec![],
            body: Box::pin(futures::stream::once(async {
                Ok(bytes::Bytes::from_static(br#"{"type":"error","error":{"type":"invalid_request_error","message":"fixture rejection"}}"#))
            })),
        })
    }
}

fn protocol(provider: &str) -> Option<ProtocolFamily> {
    Some(match provider {
        "firstParty" => ProtocolFamily::AnthropicMessages,
        "anthropicAws" => ProtocolFamily::BedrockClaude,
        "anthropicGoogleCloud" => ProtocolFamily::VertexClaude,
        "foundry" => ProtocolFamily::FoundryClaude,
        _ => return None,
    })
}
fn apply(input: &Value) {
    for (key, name) in [
        ("env", "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"),
        ("structured_env", branding::DISABLE_STRUCTURED_OUTPUTS_ENV),
    ] {
        if let Some(value) = input[key].as_str() {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
fn service(client: Arc<ModelRuntime>, capture: Arc<Capture>) -> ApiService {
    ApiService::new_with_routing(
        client,
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
    .with_interactive_session(false)
    .with_thinking(ThinkingConfig::Disabled)
}
#[tokio::test]
async fn current_structured_output_gate_reaches_preparation_and_both_physical_drivers() {
    let _environment = Environment::own();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/structured_output_2_1_288.json")).unwrap();
    let mut preparations = 0;
    for row in fixture["cases"].as_array().unwrap() {
        let input = &row["input"];
        let Some(protocol) = protocol(input["provider"].as_str().unwrap()) else {
            continue;
        };
        if input["hipaa"] == true || row["expected"].get("main").is_none() {
            continue;
        }
        apply(input);
        let model = input["model"].as_str().unwrap();
        let mut profile = profile(protocol, input["capability"].as_bool().unwrap());
        profile.models[0].request_model = model.into();
        profile.models[0].billing_model = model.into();
        profile.models[0].display_model = model.into();
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile],
        })
        .unwrap();
        for side in [false, true] {
            let mut request = schema_request();
            request.input.model = model.into();
            if side {
                request.execution.anthropic_request_kind=lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
            }
            if input["explicit"] == true {
                std::env::set_var(
                    "CLAUDE_CODE_EXTRA_BODY",
                    r#"{"output_config":{"format":{"type":"explicit_fixture"}}}"#,
                );
            } else {
                std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
            }
            let prepared = client.prepare(&request).await.unwrap();
            let expected = if side {
                &row["expected"]["side"]
            } else {
                &row["expected"]["main"]
            };
            let automatic = expected["config"]["format"]["type"] == "json_schema";
            assert_eq!(
                prepared
                    .provider_request
                    .body_json
                    .pointer("/output_config/format")
                    .is_some(),
                automatic,
                "{input}, side={side}"
            );
            assert!(matches!(
                request.input.output_format,
                lingxi_llm_client::protocol::OutputFormat::JsonSchema { .. }
            ));
            preparations += 1;
        }
    }
    assert_eq!(preparations, 36000);
    let mut captures = 0;
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::BedrockClaude,
        ProtocolFamily::VertexClaude,
        ProtocolFamily::FoundryClaude,
    ] {
        for disabled in [false, true] {
            for side in [false, true] {
                for explicit in [false, true] {
                    for stream in [false, true] {
                        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
                        std::env::set_var(
                            branding::DISABLE_STRUCTURED_OUTPUTS_ENV,
                            disabled.to_string(),
                        );
                        if explicit {
                            std::env::set_var(
                                "CLAUDE_CODE_EXTRA_BODY",
                                r#"{"output_config":{"format":{"type":"explicit_fixture"},"effort":"low"}}"#,
                            );
                        } else {
                            std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
                        }
                        let capture = Arc::new(Capture::default());
                        let client = Arc::new(
                            ModelRuntime::from_config(ClientConfig {
                                providers: vec![profile(protocol, true)],
                            })
                            .unwrap(),
                        );
                        let api = service(client, capture.clone());
                        let mut request = schema_request();
                        if side {
                            request.execution.anthropic_request_kind=lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
                        }
                        let result = if stream {
                            api.stream_request(request).await.map(|_| ())
                        } else {
                            api.execute_non_stream_request(
                                request,
                                llm_runtime::NonStreamingRequestClass::Auxiliary,
                                Default::default(),
                            )
                            .await
                            .map(|_| ())
                        };
                        assert!(result.is_err());
                        let calls = capture.0.lock().unwrap();
                        assert_eq!(calls.len(), 1);
                        let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
                        assert!(calls[0].headers.iter().any(|(name, value)| name
                            .eq_ignore_ascii_case("anthropic-version")
                            && value == "2023-06-01"));
                        assert_eq!(
                            body.pointer("/output_config/format/type")
                                .and_then(Value::as_str),
                            if explicit {
                                Some("explicit_fixture")
                            } else if disabled {
                                None
                            } else {
                                Some("json_schema")
                            },
                            "{protocol:?} disabled={disabled} side={side} explicit={explicit}"
                        );
                        let beta = calls[0]
                            .headers
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
                            .map(|(_, v)| v.as_str())
                            .unwrap_or("");
                        assert_eq!(beta.contains("structured-outputs-2025-12-15"),!disabled && (side || !explicit),"{protocol:?} disabled={disabled} side={side} explicit={explicit}: {beta}");
                        captures += 1;
                    }
                }
            }
        }
    }
    // The scoped flag is read on each attempt; the same service can resume schemas.
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    let capture = Arc::new(Capture::default());
    let api = service(
        Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(ProtocolFamily::AnthropicMessages, true)],
            })
            .unwrap(),
        ),
        capture.clone(),
    );
    for flag in ["true", "false"] {
        std::env::set_var(branding::DISABLE_STRUCTURED_OUTPUTS_ENV, flag);
        let _ = api.execute_side_query_request(schema_request()).await;
    }
    let calls = capture.0.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(serde_json::from_slice::<Value>(&calls[0].body)
        .unwrap()
        .pointer("/output_config/format")
        .is_none());
    assert!(serde_json::from_slice::<Value>(&calls[1].body)
        .unwrap()
        .pointer("/output_config/format")
        .is_some());
    drop(calls);
    // Other LLM protocols keep their schema and their unsupported-capability errors.
    std::env::set_var(branding::DISABLE_STRUCTURED_OUTPUTS_ENV, "true");
    for protocol in [
        ProtocolFamily::OpenAiChat,
        ProtocolFamily::OpenAiResponses,
        ProtocolFamily::GeminiGenerateContent,
    ] {
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile(protocol, true)],
        })
        .unwrap();
        let prepared = client.prepare(&schema_request()).await.unwrap();
        assert!(
            prepared
                .provider_request
                .body_json
                .get("response_format")
                .is_some()
                || prepared
                    .provider_request
                    .body_json
                    .pointer("/text/format")
                    .is_some()
                || prepared
                    .provider_request
                    .body_json
                    .get("generationConfig")
                    .is_some()
        );
        let unsupported = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile(protocol, false)],
        })
        .unwrap();
        assert!(matches!(
            unsupported.prepare(&schema_request()).await,
            Err(llm_runtime::LlmError::UnsupportedCapability { .. })
        ));
    }
    println!("{preparations} native preparation projections; {captures} physical driver captures; same-service flag transition; other-provider controls OK");
}
