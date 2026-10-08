//! This binary owns its environment, including the native typed boolean gate.
use async_trait::async_trait;
use futures::StreamExt;
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
            "LINGXI_EFFORT_LEVEL",
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

fn current_eligibility_probe(probe: &Value) -> bool {
    let input = &probe["input"];
    input["structured_env"].is_null()
        && input["hipaa"] == false
        && input["capability"] == true
        && input["explicit"] == false
        && probe["expected"].get("main").is_some()
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

#[tokio::test]
async fn native_kill_switch_applies_per_attempt_before_validation_and_preserves_other_providers() {
    let _environment = Environment::own();
    fast_environment_is_refreshed_for_physical_retry().await;
    current_fast_environment_reaches_transport().await;
    current_fast_catalog_reaches_transport().await;
    catalog_effort_facts_reach_controls_and_transport().await;
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/structured_output_2_1_287.json")).unwrap();
    let request = schema_request();
    let original = request.input.output_format.clone();
    let wire_fixture: Value =
        serde_json::from_str(include_str!("fixtures/structured_output_2_1_288.json")).unwrap();
    let unsupported = ModelRuntime::from_config(ClientConfig {
        providers: vec![profile(ProtocolFamily::AnthropicMessages, false)],
    })
    .unwrap();
    assert!(unsupported
        .prepare(&request)
        .await
        .unwrap()
        .provider_request
        .body_json
        .pointer("/output_config/format")
        .is_none());
    for case in fixture["cases"].as_array().unwrap().iter().filter(|case| {
        let input = &case["input"];
        input["hipaa"] == false
            && input["eligible"] == true
            && input["opus41"] == false
            && input["capability"] == true
            && input["explicit"] == false
    }) {
        match case["input"]["env"].as_str() {
            Some(value) => std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", value),
            None => std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"),
        }
        let disabled = case["expected"]["disabled"].as_bool().unwrap();
        for protocol in [
            ProtocolFamily::AnthropicMessages,
            ProtocolFamily::BedrockClaude,
            ProtocolFamily::VertexClaude,
            ProtocolFamily::FoundryClaude,
        ] {
            let provider = match protocol {
                ProtocolFamily::AnthropicMessages => "firstParty",
                ProtocolFamily::BedrockClaude => "anthropicAws",
                ProtocolFamily::VertexClaude => "anthropicGoogleCloud",
                ProtocolFamily::FoundryClaude => "foundry",
                _ => unreachable!(),
            };
            let eligible = wire_fixture["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|probe| {
                    probe["input"]["provider"] == provider
                        && probe["input"]["model"] == "claude-sonnet-4-6"
                        && probe["input"]["env"] == case["input"]["env"]
                        && current_eligibility_probe(probe)
                })
                .unwrap()["expected"]["eligible"]
                .as_bool()
                .unwrap();
            let client = ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, !disabled)],
            })
            .unwrap();
            for stream in [false, true] {
                let mut attempt = request.clone();
                attempt.stream = stream;
                let prepared = client.prepare(&attempt).await.unwrap();
                let wire = &prepared.provider_request.body_json;
                assert_eq!(
                    wire.get("output_config")
                        .and_then(|config| config.get("format")),
                    if eligible {
                        case["expected"]["main"]["config"].get("format")
                    } else {
                        None
                    },
                    "{protocol:?}, env={:?}",
                    case["input"]["env"]
                );
                assert_eq!(attempt.input.output_format, original);
            }
            if protocol == ProtocolFamily::AnthropicMessages {
                client.prepare_count_tokens(&request).await.unwrap();
            }
        }
    }

    for probe in wire_fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|probe| current_eligibility_probe(probe))
    {
        let protocol = match probe["input"]["provider"].as_str().unwrap() {
            "firstParty" => ProtocolFamily::AnthropicMessages,
            "anthropicAws" => ProtocolFamily::BedrockClaude,
            "anthropicGoogleCloud" => ProtocolFamily::VertexClaude,
            "foundry" => ProtocolFamily::FoundryClaude,
            _ => continue,
        };
        match probe["input"]["env"].as_str() {
            Some(value) => std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", value),
            None => std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"),
        }
        let model = probe["input"]["model"].as_str().unwrap();
        let mut profile = profile(protocol, true);
        profile.models[0].display_model = model.into();
        profile.models[0].request_model = model.into();
        profile.models[0].billing_model = model.into();
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile],
        })
        .unwrap();
        for stream in [false, true] {
            let mut request = schema_request();
            request.input.model = model.into();
            request.stream = stream;
            let prepared = client.prepare(&request).await.unwrap();
            assert_eq!(
                prepared
                    .provider_request
                    .body_json
                    .get("output_config")
                    .and_then(|config| config.get("format"))
                    .is_some(),
                probe["expected"]["eligible"].as_bool().unwrap(),
                "{:?}",
                probe["input"]
            );
        }
    }
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");
    let mut json_object = request.clone();
    json_object.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonObject;
    assert!(matches!(
        unsupported.prepare(&json_object).await,
        Err(llm_runtime::LlmError::UnsupportedCapability { .. })
    ));
    for protocol in [
        ProtocolFamily::OpenAiChat,
        ProtocolFamily::OpenAiResponses,
        ProtocolFamily::GeminiGenerateContent,
    ] {
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile(protocol, true)],
        })
        .unwrap();
        let prepared = client.prepare(&request).await.unwrap();
        let wire = &prepared.provider_request.body_json;
        let format = match protocol {
            ProtocolFamily::OpenAiChat => &wire["response_format"]["type"],
            ProtocolFamily::OpenAiResponses => &wire["text"]["format"]["type"],
            ProtocolFamily::GeminiGenerateContent => {
                &wire["generationConfig"]["responseFormat"]["text"]["mimeType"]
            }
            _ => unreachable!(),
        };
        assert_eq!(
            format,
            if protocol == ProtocolFamily::GeminiGenerateContent {
                "application/json"
            } else {
                "json_schema"
            },
            "{protocol:?}: {wire}"
        );
        assert_eq!(request.input.output_format, original);
        let unsupported = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile(protocol, false)],
        })
        .unwrap();
        assert!(matches!(
            unsupported.prepare(&request).await,
            Err(llm_runtime::LlmError::UnsupportedCapability { .. })
        ));
    }

    // The same immutable schema can be routed from Anthropic to OpenAI.
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![
                profile(ProtocolFamily::AnthropicMessages, false),
                profile(ProtocolFamily::OpenAiChat, true),
            ],
        })
        .unwrap(),
    );
    let primary = request.clone().with_profile("anthropic");
    let fallback = request.clone().with_profile("openai");
    assert!(client
        .prepare(&primary)
        .await
        .unwrap()
        .provider_request
        .body_json
        .get("output_config")
        .is_none());
    assert_eq!(
        client
            .prepare(&fallback)
            .await
            .unwrap()
            .provider_request
            .body_json["response_format"]["type"],
        "json_schema"
    );

    let capture = Arc::new(Capture::default());
    let service = ApiService::new_with_routing(
        client,
        capture.clone(),
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
    .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }))
    .with_thinking(ThinkingConfig::Disabled);
    // All public side-query paths reach the SDK transport without a schema.
    service
        .execute_side_query_request(primary.clone())
        .await
        .unwrap_err();
    service
        .stream_json_schema(
            "claude-sonnet-4-6",
            Some("anthropic"),
            None,
            vec![],
            json!({"type":"object"}),
            Some(16),
            None,
        )
        .await
        .err()
        .expect("fixture error");
    service
        .stream_json_schema_with_thinking(
            "claude-sonnet-4-6",
            Some("anthropic"),
            None,
            vec![],
            json!({"type":"object"}),
            Some(16),
            None,
            None,
            None,
            Some("session_title"),
        )
        .await
        .err()
        .expect("fixture error");
    for wire in capture.0.lock().unwrap().iter() {
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        assert!(body.get("output_config").is_none(), "{body}");
        assert!(!wire
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("anthropic-beta")
                && value.contains("structured-outputs")));
    }
    assert_eq!(capture.0.lock().unwrap().len(), 3);
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_BODY",
        r#"{"output_config":{"format":{"type":"explicit_fixture"},"effort":"low"}}"#,
    );
    service
        .execute_side_query_request(primary)
        .await
        .unwrap_err();
    {
        let wires = capture.0.lock().unwrap();
        let body: Value = serde_json::from_slice(&wires.last().unwrap().body).unwrap();
        assert_eq!(
            body["output_config"],
            json!({"format":{"type":"explicit_fixture"},"effort":"low"})
        );
    }
    // Typed body kind is independent of the auxiliary dispatch category.
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    let schema_client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![profile(ProtocolFamily::AnthropicMessages, true)],
        })
        .unwrap(),
    );
    let schema_service = ApiService::new_with_routing(
        schema_client,
        capture.clone(),
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
    .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }))
    .with_thinking(ThinkingConfig::Disabled);
    for kind in [
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery,
    ] {
        for explicit in [
            None,
            Some(json!({"format":null,"effort":"low"})),
            Some(Value::Null),
            Some(json!({})),
        ] {
            match explicit.as_ref() {
                Some(config) => std::env::set_var(
                    "CLAUDE_CODE_EXTRA_BODY",
                    json!({"output_config":config}).to_string(),
                ),
                None => std::env::remove_var("CLAUDE_CODE_EXTRA_BODY"),
            }
            let mut request = schema_request().with_profile("anthropic");
            request.execution.anthropic_request_kind = kind;
            let before = capture.0.lock().unwrap().len();
            schema_service
                .execute_non_stream_request(
                    request,
                    llm_runtime::NonStreamingRequestClass::Auxiliary,
                    Default::default(),
                )
                .await
                .unwrap_err();
            let wires = capture.0.lock().unwrap();
            assert_eq!(wires.len(), before + 1, "must reach SDK transport");
            let body: Value = serde_json::from_slice(&wires.last().unwrap().body).unwrap();
            let should_beta = kind == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery || !explicit.as_ref().and_then(Value::as_object).is_some_and(|config|config.contains_key("format"));
            let beta = wires
                .last()
                .unwrap()
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            assert_eq!(
                beta.split(',')
                    .filter(|value| *value == "structured-outputs-2025-12-15")
                    .count(),
                usize::from(should_beta)
            );
            if kind == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery {
                if let Some(config) = explicit { assert_eq!(&body["output_config"], &config); }
                else { assert_eq!(body["output_config"]["format"]["type"], "json_schema"); }
            } else if explicit.as_ref().and_then(Value::as_object).is_some_and(|config|config.contains_key("format")) {
                assert_eq!(body["output_config"]["format"], Value::Null);
            } else { assert_eq!(body["output_config"]["format"]["type"], "json_schema"); }
        }
    }
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_BODY",
        json!({"output_config":"💥"}).to_string(),
    );
    let before = capture.0.lock().unwrap().len();
    schema_service
        .execute_non_stream_request(
            schema_request().with_profile("anthropic"),
            llm_runtime::NonStreamingRequestClass::Auxiliary,
            Default::default(),
        )
        .await
        .unwrap_err();
    {
        let wires = capture.0.lock().unwrap();
        assert_eq!(wires.len(), before + 1);
        let wire = std::str::from_utf8(&wires.last().unwrap().body).unwrap();
        assert!(wire.contains(r#""0":"\ud83d""#), "{wire}");
        assert!(wire.contains(r#""1":"\udca5""#), "{wire}");
    }
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
        ProtocolFamily::BedrockClaude,
        ProtocolFamily::VertexClaude,
    ] {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, true)],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client.clone(),
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", "\u{FEFF}{\"anthropic_beta\":[\"AFK-MODE\",\"custom\"],\"metadata\":{\"user_id\":\"{\\\"tk\\\":\\\"secret\\\",\\\"id\\\":\\\"new\\\"}\"}}");
        service
            .execute_non_stream_request(
                LlmRequest::new("claude-sonnet-4-6").with_user_text("hi"),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        {
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), 1, "{protocol:?}");
            let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(body["anthropic_beta"][0], "custom", "{protocol:?}");
            assert_eq!(
                body["metadata"]["user_id"], r#"{"id":"new"}"#,
                "{protocol:?}"
            );
        }
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"betas":{"toString":null}}"#);
        assert!(matches!(
            client
                .prepare(&LlmRequest::new("claude-sonnet-4-6").with_user_text("hi"))
                .await,
            Err(llm_runtime::LlmError::InvalidRequest { .. })
        ));
        assert_eq!(
            capture.0.lock().unwrap().len(),
            1,
            "invalid coercion cannot dispatch"
        );
    }
    // The experimental-beta switch does not suppress explicit extra body.
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
    ] {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, true)],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        for betas in [
            json!(["AFK-MODE", "custom"]),
            Value::Null,
            json!("keep,other"),
        ] {
            std::env::set_var(
                "CLAUDE_CODE_EXTRA_BODY",
                json!({"betas":betas,"user_profile_id":" profile ","workspace_id":" workspace "})
                    .to_string(),
            );
            service
                .execute_non_stream_request(
                    schema_request(),
                    llm_runtime::NonStreamingRequestClass::Auxiliary,
                    Default::default(),
                )
                .await
                .unwrap_err();
            let requests = capture.0.lock().unwrap();
            let request = requests.last().unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            for key in ["betas", "user_profile_id", "workspace_id"] {
                assert!(body.get(key).is_none(), "{protocol:?} {body}");
            }
            let header = |name| {
                request
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.as_str())
            };
            assert_eq!(header("anthropic-user-profile-id"), Some("profile"));
            assert_eq!(header("anthropic-workspace-id"), Some("workspace"));
            let expected = if betas.is_null() {
                None
            } else if betas.is_array() {
                Some("custom")
            } else {
                Some("keep,other")
            };
            assert_eq!(header("anthropic-beta"), expected, "{protocol:?}");
        }
    }
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    // The current native SDK px normalizer runs after extra-body composition.
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
        ProtocolFamily::BedrockClaude,
        ProtocolFamily::VertexClaude,
    ] {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, true)],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", json!({"output_format":{"type":"json_schema","schema":{"type":"object"}},"output_config":{"effort":"low"}}).to_string());
        service
            .execute_non_stream_request(
                LlmRequest::new("claude-sonnet-4-6").with_user_text("hi"),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        {
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), 1, "{protocol:?}");
            let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert!(body.get("output_format").is_none(), "{protocol:?} {body}");
            assert_eq!(body["output_config"]["format"]["type"], "json_schema");
            assert_eq!(
                body.pointer("/output_config/effort"),
                matches!(
                    protocol,
                    ProtocolFamily::AnthropicMessages | ProtocolFamily::FoundryClaude
                )
                .then_some(&json!("low"))
            );
        }
        std::env::set_var(
            "CLAUDE_CODE_EXTRA_BODY",
            r#"{"output_format":{"type":"new"},"output_config":{"format":{"type":"existing"}}}"#,
        );
        let error = service
            .execute_non_stream_request(
                LlmRequest::new("claude-sonnet-4-6").with_user_text("hi"),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, llm_runtime::LlmError::InvalidRequest { ref message } if message == "Both output_format and output_config.format were provided. Please use only output_config.format (output_format is deprecated)."),
            "{protocol:?} {error:?}"
        );
        assert_eq!(
            capture.0.lock().unwrap().len(),
            1,
            "conflict cannot dispatch"
        );
    }
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
        ProtocolFamily::BedrockClaude,
        ProtocolFamily::VertexClaude,
    ] {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, true)],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client.clone(),
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", json!({"numbers":{"large":1e21,"small":1e-7,"zero":-0.0,"integer":u64::MAX,"2":1.0,"0":-0.0,"01":2.0}}).to_string());
        let request = LlmRequest::new("claude-sonnet-4-6").with_user_text("hi");
        let prepared = client.prepare(&request).await.unwrap();
        assert_eq!(
            prepared.provider_request.json_encoding,
            lingxi_llm_client::exact_json::JsonEncoding::JavaScript
        );
        service
            .execute_non_stream_request(
                request,
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let wire = std::str::from_utf8(&requests[0].body).unwrap();
        assert!(wire.contains(r#""numbers":{"0":0,"2":1,"large":1e+21,"small":1e-7,"zero":0,"integer":18446744073709552000,"01":2}"#), "{protocol:?} {wire}");
    }
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    for protocol in [
        ProtocolFamily::OpenAiChat,
        ProtocolFamily::OpenAiResponses,
        ProtocolFamily::GeminiGenerateContent,
    ] {
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile(protocol, true)],
        })
        .unwrap();
        let prepared = client
            .prepare(&LlmRequest::new("claude-sonnet-4-6").with_user_text("hi"))
            .await
            .unwrap();
        assert_eq!(
            prepared.provider_request.json_encoding,
            lingxi_llm_client::exact_json::JsonEncoding::Serde
        );
    }
    // Native captures prove these URI/body rules after actual SDK serialization.
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
        ProtocolFamily::BedrockClaude,
        ProtocolFamily::VertexClaude,
    ] {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, true)],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", json!({"metadata":{"user_id":r#"{"tk":"secret","id":"\ud800","\udc00":"value"}"#},"extension":true}).to_string());
        let mut request = LlmRequest::new("claude-sonnet-4-6").with_user_text("hi");
        request.input.metadata = json!({"user_id":"base-identity"});
        request
            .input
            .system
            .push(lingxi_llm_client::protocol::SystemBlock {
                text: "WIRE_SYSTEM".into(),
            });
        service
            .execute_non_stream_request(
                request,
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let wire = &requests[0];
        if matches!(
            protocol,
            ProtocolFamily::AnthropicMessages | ProtocolFamily::FoundryClaude
        ) {
            assert_eq!(
                url::Url::parse(&wire.url).unwrap().query(),
                Some("beta=true")
            );
        }
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(
            body["metadata"]["user_id"],
            r#"{"id":"\ud800","\udc00":"value"}"#
        );
        let keys: Vec<_> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            keys.iter().position(|key| *key == "messages")
                < keys.iter().position(|key| *key == "system"),
            "{protocol:?} {keys:?}"
        );
        assert!(
            keys.iter().position(|key| *key == "metadata")
                < keys.iter().position(|key| *key == "max_tokens"),
            "{protocol:?} {keys:?}"
        );
        assert!(
            keys.iter().position(|key| *key == "max_tokens")
                < keys.iter().position(|key| *key == "extension"),
            "{protocol:?} {keys:?}"
        );
    }
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
    ] {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile(protocol, true)],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        std::env::set_var(
            "CLAUDE_CODE_EXTRA_BODY",
            r#"{"stream":false,"extension":true}"#,
        );
        let request = LlmRequest::new("claude-sonnet-4-6").with_user_text("hi");
        if let Ok(stream) = service.stream_request(request).await {
            let _events = stream.collect::<Vec<_>>().await;
        }
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            url::Url::parse(&requests[0].url).unwrap().query(),
            Some("beta=true")
        );
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["stream"], true,
            "main stream call owns its final stream flag"
        );
    }
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    let beta_fixture: Value =
        serde_json::from_str(include_str!("fixtures/betas_2_1_287.json")).unwrap();
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::FoundryClaude,
        ProtocolFamily::VertexClaude,
    ] {
        let native_provider = match protocol {
            ProtocolFamily::AnthropicMessages => "firstParty",
            ProtocolFamily::FoundryClaude => "foundry",
            _ => "vertex",
        };
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-1",
            "claude-opus-4-0",
            "claude-sonnet-5",
        ] {
            let expected = &beta_fixture["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| {
                    case["input"]["provider"] == native_provider
                        && case["input"]["model"] == model
                        && case["input"]["env"] == json!({})
                        && case["input"]["interactive"] == false
                })
                .unwrap()["expected"];
            for side in [false, true] {
                for explicit in [false, true] {
                    let mut row = profile(protocol, true);
                    row.models[0].display_model = model.into();
                    row.models[0].request_model = model.into();
                    row.models[0].billing_model = model.into();
                    let client = Arc::new(
                        ModelRuntime::from_config(ClientConfig {
                            providers: vec![row],
                        })
                        .unwrap(),
                    );
                    let capture = Arc::new(Capture::default());
                    let service = ApiService::new_with_routing(
                        client,
                        capture.clone(),
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
                    .with_fast_policy_source(Arc::new(|| {
                        llm_runtime::model::fast_admission::Policy {
                            cached_org_enabled: true,
                            ..Default::default()
                        }
                    }))
                    .with_thinking(ThinkingConfig::Disabled)
                    .with_interactive_session(false);
                    if explicit {
                        std::env::set_var(
                            "CLAUDE_CODE_EXTRA_BODY",
                            r#"{"output_config":{"effort":"low"}}"#,
                        );
                    } else {
                        std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
                    }
                    let mut attempt = LlmRequest::new(model).with_user_text("hi");
                    if side {
                        attempt.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
                    }
                    service
                        .execute_non_stream_request(
                            attempt,
                            llm_runtime::NonStreamingRequestClass::Auxiliary,
                            Default::default(),
                        )
                        .await
                        .unwrap_err();
                    let wires = capture.0.lock().unwrap();
                    assert_eq!(wires.len(), 1);
                    let header = wires[0]
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
                        .map(|(_, value)| value.as_str())
                        .unwrap_or("");
                    let expected_key = if side || explicit {
                        "base"
                    } else {
                        "default_main"
                    };
                    assert_eq!(
                        header,
                        expected[expected_key].as_str().unwrap(),
                        "{native_provider} {model} side={side} explicit={explicit}"
                    );
                    let body: Value = serde_json::from_slice(&wires[0].body).unwrap();
                    let expected_effort = if explicit && (side || expected["effort"] == true) {
                        Some(json!("low"))
                    } else if !side && !explicit && expected["effort"] == true {
                        Some(json!("high"))
                    } else {
                        None
                    };
                    assert_eq!(body.pointer("/output_config/effort"), expected_effort.as_ref(), "native YMe body: {native_provider} {model} side={side} explicit={explicit}");
                }
            }
        }
    }
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    // Native jb resolution is selected before SDK validation and sealed through
    // the same production request path, with no provider-neutral input mutation.
    for (env, primary, turn, hook, settings_cap, provider_cap, side, model, disabled, expected) in [
        (
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            Some("high"),
        ),
        (
            Some("low"),
            Some("max"),
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            Some("low"),
        ),
        (
            Some("AUTO"),
            Some("low"),
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            None,
        ),
        (
            Some("unset"),
            None,
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            None,
        ),
        (
            Some("0"),
            None,
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            None,
        ),
        (
            Some("MED"),
            None,
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            Some("medium"),
        ),
        (
            Some("xhigh"),
            None,
            None,
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            Some("high"),
        ),
        (
            None,
            Some("low"),
            Some("max"),
            None,
            None,
            None,
            false,
            "claude-sonnet-4-6",
            false,
            Some("max"),
        ),
        (
            Some("low"),
            None,
            None,
            Some("max"),
            None,
            None,
            true,
            "claude-sonnet-4-6",
            false,
            Some("max"),
        ),
        (
            Some("auto"),
            Some("low"),
            None,
            None,
            Some("high"),
            None,
            false,
            "claude-sonnet-4-6",
            false,
            Some("high"),
        ),
        (
            Some("auto"),
            None,
            None,
            None,
            None,
            Some("low"),
            false,
            "claude-sonnet-4-6",
            false,
            None,
        ),
        (
            None,
            None,
            None,
            Some("max"),
            None,
            None,
            true,
            "claude-opus-5",
            true,
            Some("high"),
        ),
    ] {
        match env {
            Some(value) => std::env::set_var("LINGXI_EFFORT_LEVEL", value),
            None => std::env::remove_var("LINGXI_EFFORT_LEVEL"),
        };
        let mut row = profile(ProtocolFamily::AnthropicMessages, true);
        row.models[0].display_model = model.into();
        row.models[0].request_model = model.into();
        row.models[0].billing_model = model.into();
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![row],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled)
        .with_interactive_session(false);
        let mut attempt = LlmRequest::new(model).with_user_text("hi");
        if let Some(level) = primary {
            attempt.set_effort(Some(json!(level))).unwrap();
        }
        attempt.execution.effort_state.turn = turn.map(|value| json!(value));
        attempt.execution.effort_state.hook = hook.map(|value| json!(value));
        attempt.execution.effort_state.settings_cap = settings_cap.map(str::to_owned);
        attempt.execution.effort_state.provider_cap = provider_cap.map(str::to_owned);
        attempt.execution.side_thinking_disabled = disabled;
        if side {
            attempt.execution.anthropic_request_kind=lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        }
        let original = attempt.input.thinking.clone();
        service
            .execute_non_stream_request(
                attempt.clone(),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(attempt.input.thinking, original);
        let wires = capture.0.lock().unwrap();
        assert_eq!(wires.len(), 1);
        let body: Value = serde_json::from_slice(&wires[0].body).unwrap();
        assert_eq!(
            body.pointer("/output_config/effort")
                .and_then(Value::as_str),
            expected,
            "env={env:?} model={model} side={side}"
        );
        let beta = wires[0]
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| value.as_str())
            .unwrap();
        assert_eq!(
            beta.split(',').any(|value| value == "effort-2025-11-24"),
            env != Some("0"),
            "env={env:?} model={model} side={side}"
        );
    }
    std::env::set_var("LINGXI_EFFORT_LEVEL", "high");
    let mut row = profile(ProtocolFamily::AnthropicMessages, true);
    row.auth = AuthStrategy::ApiKey;
    row.credential = CredentialConfig::HostManaged {
        id: "effort-snapshot".into(),
    };
    let credentials = Arc::new(EffortEnvironmentCredentials::default());
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![row],
        })
        .unwrap()
        .with_credential_provider(credentials.clone()),
    );
    let capture = Arc::new(Capture::default());
    let service = ApiService::new_with_routing(
        client,
        capture.clone(),
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
    .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }))
    .with_thinking(ThinkingConfig::Disabled);
    for index in 0..2 {
        let snapshot = service
            .effort_command_snapshot("claude-sonnet-4-6", None)
            .unwrap()
            .unwrap();
        let environment = std::env::var("LINGXI_EFFORT_LEVEL").ok();
        assert_eq!(
            snapshot.displayed(environment.as_deref()),
            if index == 0 { "high" } else { "low" }
        );
        assert_eq!(
            credentials.0.load(std::sync::atomic::Ordering::SeqCst),
            index,
            "command query must not authenticate"
        );
        assert!(
            capture.0.lock().unwrap().len() == index,
            "command query must not use transport"
        );
        service
            .execute_non_stream_request(
                LlmRequest::new("claude-sonnet-4-6").with_user_text("snapshot"),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
    }
    assert_eq!(credentials.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    let wires = capture.0.lock().unwrap();
    for (wire, expected) in wires.iter().zip(["high", "low"]) {
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(
            body["output_config"]["effort"], expected,
            "authentication must not change this preparation's selection"
        );
    }
    drop(wires);
    std::env::remove_var("LINGXI_EFFORT_LEVEL");
    // Real admitted files retain earlier caps, refresh on each preparation,
    // and resolve a picker alias through the selected SDK profile.
    struct SettingsDir(std::path::PathBuf);
    impl SettingsDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for SettingsDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let settings_dir = SettingsDir(std::env::temp_dir().join(format!(
        "harness-effort-settings-{}",
        lingxi_core::types::MessageId::new()
    )));
    std::fs::create_dir_all(settings_dir.path()).unwrap();
    let user_path = settings_dir.path().join("user-settings.json");
    std::fs::write(&user_path, r#"{"maxEffortLevel":"low"}"#).unwrap();
    let flag: lingxi_core::settings::SettingsJson =
        serde_json::from_str(r#"{"maxEffortLevel":"max"}"#).unwrap();
    let mut row = profile(ProtocolFamily::AnthropicMessages, true);
    row.models[0].aliases = vec!["sonnet".into()];
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![row],
        })
        .unwrap(),
    );
    let capture = Arc::new(Capture::default());
    let admitted = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let sample_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let service = ApiService::new_with_routing(
        client,
        capture.clone(),
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
    .with_effort_settings_source({
        let path = user_path.clone();
        let project_dir = settings_dir.path().to_path_buf();
        let admitted = admitted.clone();
        let sample_count = sample_count.clone();
        Arc::new(move || {
            sample_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !admitted.load(std::sync::atomic::Ordering::SeqCst) {
                return Vec::new();
            }
            lingxi_core::settings::Settings::load_with_layers_from_user_path(
                lingxi_core::settings::LoadInputs {
                    env: &Default::default(),
                    project_dir: &project_dir,
                    defaults: Default::default(),
                },
                lingxi_core::settings::FileLayerScope {
                    include_user: true,
                    include_project: false,
                    include_local: false,
                },
                lingxi_core::settings::SupplementalLayers {
                    cli_layer: Some(&flag),
                    managed_layers: &[],
                },
                Some(&path),
            )
            .unwrap()
            .effort_layers
        })
    })
    .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }))
    .with_thinking(ThinkingConfig::Disabled);
    for (document, env, expected, allow, stream) in [
        (
            r#"{"maxEffortLevel":"low"}"#,
            "max",
            Some("low"),
            true,
            false,
        ),
        (
            r#"{"maxEffortLevel":"low","modelSettings":{"sonnet":{"maxEffortLevel":"max"}}}"#,
            "max",
            Some("max"),
            true,
            false,
        ),
        (
            r#"{"maxEffortLevel":"high","modelSettings":{"claude-sonnet-4":{"maxEffortLevel":"low"}}}"#,
            "max",
            Some("high"),
            true,
            true,
        ),
        (
            r#"{"maxEffortLevel":"medium"}"#,
            "auto",
            Some("medium"),
            true,
            false,
        ),
        (r#"{"maxEffortLevel":"low"}"#, "auto", None, false, true),
        (
            r#"{"maxEffortLevel":"low"}"#,
            "17",
            Some("low"),
            true,
            false,
        ),
        (
            r#"{"maxEffortLevel":"high","modelSettings":{"sonnet[1m]":{"maxEffortLevel":"low"}}}"#,
            "max",
            Some("low"),
            true,
            false,
        ),
        (
            r#"{"maxEffortLevel":"high","modelSettings":{"\uFEFFsonnet\uFEFF":{"maxEffortLevel":"low"}}}"#,
            "max",
            Some("low"),
            true,
            false,
        ),
        (
            r#"{"maxEffortLevel":"high","modelSettings":{"\u0085sonnet\u0085":{"maxEffortLevel":"low"}}}"#,
            "max",
            Some("high"),
            true,
            false,
        ),
    ] {
        std::fs::write(&user_path, document).unwrap();
        admitted.store(allow, std::sync::atomic::Ordering::SeqCst);
        std::env::set_var("LINGXI_EFFORT_LEVEL", env);
        let request = LlmRequest::new("sonnet").with_user_text("admitted settings");
        if stream {
            service.stream_request(request).await.err().unwrap();
        } else {
            service
                .execute_non_stream_request(
                    request,
                    llm_runtime::NonStreamingRequestClass::Auxiliary,
                    Default::default(),
                )
                .await
                .unwrap_err();
        }
        let wires = capture.0.lock().unwrap();
        let body: Value = serde_json::from_slice(&wires.last().unwrap().body).unwrap();
        assert_eq!(
            body.pointer("/output_config/effort")
                .and_then(Value::as_str),
            expected,
            "{document}, {env}, admitted={allow}"
        );
    }
    assert_eq!(sample_count.load(std::sync::atomic::Ordering::SeqCst), 10);
    std::env::remove_var("LINGXI_EFFORT_LEVEL");
    // A Foundry deployment's wire alias is not its underlying model identity.
    for (underlying, expected) in [
        ("claude-sonnet-4-0", None),
        ("claude-opus-5-5", Some("low")),
    ] {
        let mut row = profile(ProtocolFamily::FoundryClaude, true);
        row.models[0].display_model = "production-deployment".into();
        row.models[0].request_model = "production-deployment".into();
        row.models[0].billing_model = underlying.into();
        row.wire_profile = Some(serde_json::from_value(json!({
            "provider_id":"foundry-claude", "profile_name":row.profile_name,
            "base_url":row.base_url, "protocol":ProtocolFamily::FoundryClaude, "auth":"none",
            "models":[{"display_model":"production-deployment", "request_model":"production-deployment", "billing_model":underlying,
                "foundry":{"hosting":"azure", "model_id":underlying}}]
        })).unwrap());
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![row],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let settings: lingxi_core::settings::SettingsJson =
            serde_json::from_value(json!({"modelSettings":{underlying:{"maxEffortLevel":"low"}}}))
                .unwrap();
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_effort_settings_source(Arc::new(move || {
            vec![lingxi_core::host::effort::EffortSettingsLayer::new(
                lingxi_core::settings::tracer::Source::Managed,
                &settings,
            )]
        }));
        service
            .execute_non_stream_request(
                LlmRequest::new("production-deployment").with_user_text("underlying identity"),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        let wires = capture.0.lock().unwrap();
        let body: Value = serde_json::from_slice(&wires.last().unwrap().body).unwrap();
        assert_eq!(body["model"], "production-deployment");
        assert_eq!(
            body.pointer("/output_config/effort")
                .and_then(Value::as_str),
            expected,
            "{underlying}"
        );
    }
    // Q/kd inheritance is a session boot snapshot, while K caps remain live.
    use lingxi_core::host::effort_table::{SessionEffort, TableOptions};
    std::fs::write(
        &user_path,
        r#"{"effortLevel":"low","modelSettings":{"sonnet":{"effortLevel":"medium"}}}"#,
    )
    .unwrap();
    let mut row = profile(ProtocolFamily::AnthropicMessages, true);
    row.models[0].aliases = vec!["sonnet".into()];
    for (model, alias) in [
        ("claude-opus-5-5", "opus"),
        ("claude-opus-4-8", "older"),
        ("claude-opus-6", "future"),
    ] {
        let mut entry = row.models[0].clone();
        entry.display_model = model.into();
        entry.request_model = model.into();
        entry.billing_model = model.into();
        entry.aliases = vec![alias.into()];
        row.models.push(entry);
    }
    let make_settings_service = |options: TableOptions| {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![row.clone()],
            })
            .unwrap(),
        );
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
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
        .with_effort_table_options(options)
        .with_effort_settings_source({
            let path = user_path.clone();
            let directory = settings_dir.path().to_path_buf();
            Arc::new(move || {
                lingxi_core::settings::Settings::load_with_layers_from_user_path(
                    lingxi_core::settings::LoadInputs {
                        env: &Default::default(),
                        project_dir: &directory,
                        defaults: Default::default(),
                    },
                    lingxi_core::settings::FileLayerScope {
                        include_user: true,
                        include_project: false,
                        include_local: false,
                    },
                    Default::default(),
                    Some(&path),
                )
                .unwrap()
                .effort_layers
            })
        })
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled);
        (service, capture)
    };
    let (inherit_service, inherit_capture) = make_settings_service(TableOptions::from_global(
        &json!({"firstStartVersion":"2.1.287"}),
        None,
    ));
    for (model, state, side, supplied, expected) in [
        (
            "sonnet",
            SessionEffort::Inherit,
            false,
            None,
            Some("medium"),
        ),
        ("opus", SessionEffort::Inherit, false, None, Some("medium")),
        ("older", SessionEffort::Inherit, false, None, Some("low")),
        ("future", SessionEffort::Inherit, false, None, Some("high")),
        ("sonnet", SessionEffort::Default, false, None, Some("high")),
        (
            "sonnet",
            SessionEffort::Level(json!("low")),
            false,
            None,
            Some("low"),
        ),
        ("sonnet", SessionEffort::Level(json!(0)), false, None, None),
        (
            "sonnet",
            SessionEffort::Inherit,
            false,
            Some("low"),
            Some("low"),
        ),
        ("sonnet", SessionEffort::Inherit, true, None, None),
        (
            "sonnet",
            SessionEffort::Level(json!("low")),
            true,
            None,
            None,
        ),
        (
            "sonnet",
            SessionEffort::Inherit,
            true,
            Some("high"),
            Some("high"),
        ),
    ] {
        inherit_service.set_session_effort(state);
        let mut request = LlmRequest::new(model).with_user_text("session selection");
        if side {
            request.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        }
        if let Some(level) = supplied {
            request.set_effort(Some(json!(level))).unwrap();
        }
        inherit_service
            .execute_non_stream_request(
                request,
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        let wires = inherit_capture.0.lock().unwrap();
        let body: Value = serde_json::from_slice(&wires.last().unwrap().body).unwrap();
        assert_eq!(
            body.pointer("/output_config/effort")
                .and_then(Value::as_str),
            expected,
            "{model} side={side}"
        );
    }
    inherit_service.set_session_effort(SessionEffort::Inherit);
    for (document, environment, expected) in [
        (
            r#"{"effortLevel":"high","modelSettings":{"sonnet":{"effortLevel":"low"}}}"#,
            None,
            Some("medium"),
        ),
        (
            r#"{"maxEffortLevel":"low","modelSettings":{"sonnet":{"effortLevel":"high"}}}"#,
            None,
            Some("low"),
        ),
        (
            r#"{"maxEffortLevel":"medium"}"#,
            Some("auto"),
            Some("medium"),
        ),
        (r#"{}"#, Some("auto"), None),
        (r#"{}"#, Some("low"), Some("low")),
        (r#"{"maxEffortLevel":"high"}"#, Some("auto"), Some("high")),
    ] {
        std::fs::write(&user_path, document).unwrap();
        match environment {
            Some(value) => std::env::set_var("LINGXI_EFFORT_LEVEL", value),
            None => std::env::remove_var("LINGXI_EFFORT_LEVEL"),
        }
        inherit_service
            .execute_non_stream_request(
                LlmRequest::new("sonnet").with_user_text("snapshot and live caps"),
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        let wires = inherit_capture.0.lock().unwrap();
        let body: Value = serde_json::from_slice(&wires.last().unwrap().body).unwrap();
        assert_eq!(
            body.pointer("/output_config/effort")
                .and_then(Value::as_str),
            expected,
            "{document} {environment:?}"
        );
    }
    std::env::remove_var("LINGXI_EFFORT_LEVEL");
    std::fs::write(&user_path, r#"{"effortLevel":"low"}"#).unwrap();
    let (fresh, fresh_capture) = make_settings_service(TableOptions::default());
    fresh
        .execute_non_stream_request(
            LlmRequest::new("older").with_user_text("first start excludes root effort"),
            llm_runtime::NonStreamingRequestClass::Auxiliary,
            Default::default(),
        )
        .await
        .unwrap_err();
    let body: Value =
        serde_json::from_slice(&fresh_capture.0.lock().unwrap().last().unwrap().body).unwrap();
    assert_eq!(body["output_config"]["effort"], "high");
    std::fs::write(
        &user_path,
        r#"{"modelSettings":{"sonnet":{"effortLevel":"low"}}}"#,
    )
    .unwrap();
    let (fresh, fresh_capture) = make_settings_service(TableOptions::default());
    fresh
        .execute_non_stream_request(
            LlmRequest::new("sonnet").with_user_text("new session captures current defaults"),
            llm_runtime::NonStreamingRequestClass::Auxiliary,
            Default::default(),
        )
        .await
        .unwrap_err();
    let body: Value =
        serde_json::from_slice(&fresh_capture.0.lock().unwrap().last().unwrap().body).unwrap();
    assert_eq!(body["output_config"]["effort"], "low");
    let mut fallback_profile = profile(ProtocolFamily::OpenAiChat, true);
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "true");
    fallback_profile.models[0].display_model = "gpt-fallback".into();
    fallback_profile.models[0].request_model = "gpt-fallback".into();
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![
                profile(ProtocolFamily::AnthropicMessages, false),
                fallback_profile,
            ],
        })
        .unwrap(),
    );
    let capture = Arc::new(FallbackCapture::default());
    let service = ApiService::new_with_routing(
        client,
        capture.clone(),
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
    .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }))
    .with_thinking(ThinkingConfig::Disabled);
    service
        .execute_non_stream_request(
            request,
            llm_runtime::NonStreamingRequestClass::Auxiliary,
            llm_runtime::NonStreamingRetryOptions {
                initial_consecutive_overloaded: Some(2),
                fallback: llm_runtime::FallbackPolicy::Models(vec!["gpt-fallback".into()]),
            },
        )
        .await
        .unwrap();
    let wires = capture.0.lock().unwrap();
    assert_eq!(wires.len(), 2);
    let primary: Value = serde_json::from_slice(&wires[0].body).unwrap();
    let fallback: Value = serde_json::from_slice(&wires[1].body).unwrap();
    assert!(primary.pointer("/output_config/format").is_none());
    assert_eq!(primary["output_config"]["effort"], "high");
    assert_eq!(fallback["response_format"]["type"], "json_schema");
    assert_eq!(
        fallback["response_format"]["json_schema"]["schema"]["required"],
        json!(["ok"])
    );
}

#[derive(Default)]
struct FallbackCapture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
#[async_trait]
impl Transport for FallbackCapture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let fallback = request.url.ends_with("/chat/completions");
        self.0.lock().unwrap().push(request);
        let body = if fallback {
            json!({"id":"fixture","model":"gpt-fallback","choices":[{"index":0,"message":{"role":"assistant","content":"{\"ok\":true}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2,"total_tokens":4}})
        } else {
            json!({"type":"error","error":{"type":"overloaded_error","message":"fixture overload"}})
        };
        Ok(lingxi_llm_client::StreamResponse {
            status: if fallback { 200 } else { 529 },
            headers: vec![],
            body: Box::pin(futures::stream::once(async move {
                Ok(bytes::Bytes::from(serde_json::to_vec(&body).unwrap()))
            })),
        })
    }
}

#[derive(Debug, Default)]
struct EffortEnvironmentCredentials(std::sync::atomic::AtomicUsize);
impl llm_runtime::CredentialProvider for EffortEnvironmentCredentials {
    fn load<'a>(
        &'a self,
        _scope: &'a llm_runtime::CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::Credential, llm_runtime::LlmError>> {
        Box::pin(async move {
            let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::env::set_var("LINGXI_EFFORT_LEVEL", if call == 0 { "low" } else { "max" });
            Ok(llm_runtime::Credential::ApiKey(
                "synthetic-effort-key".into(),
            ))
        })
    }
}

// Directory traffic and request traffic both pass through the SDK. This
// fixture uses a mock directory response; it is not live account acceptance.
struct EffortDirectory(Value);
#[async_trait]
impl Transport for EffortDirectory {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        assert!(
            request.url.contains("/v1/models"),
            "directory stays in the SDK: {}",
            request.url
        );
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&self.0).unwrap().into(),
        }
        .into())
    }
}

async fn catalog_effort_facts_reach_controls_and_transport() {
    use lingxi_core::host::effort_table::SessionEffort;
    use lingxi_llm_client::protocol::{EffortSupport, ReasoningEffort};
    let temp = std::env::temp_dir().join(format!("harness-effort-catalog-{}", std::process::id()));
    for (index, effort, primary, default, carried, expected) in [
        (
            0,
            json!({"supported":false,"max":{"supported":true}}),
            Some("max"),
            None,
            None,
            None,
        ),
        (
            1,
            json!({"supported":true,"max":{"supported":false},"xhigh":{"supported":false}}),
            Some("max"),
            None,
            None,
            Some("high"),
        ),
        (
            2,
            json!({"supported":true,"max":{"supported":true},"xhigh":{"supported":true}}),
            Some("xhigh"),
            None,
            None,
            Some("xhigh"),
        ),
        (
            3,
            json!({"max":{"supported":false}}),
            Some("max"),
            None,
            None,
            Some("high"),
        ),
        (
            4,
            json!({"supported":true}),
            None,
            Some(ReasoningEffort::Medium),
            None,
            Some("medium"),
        ),
        (
            5,
            json!({"supported":true}),
            None,
            Some(ReasoningEffort::Medium),
            Some("low"),
            Some("low"),
        ),
        (
            6,
            json!({"supported":true}),
            Some("high"),
            Some(ReasoningEffort::Medium),
            None,
            Some("high"),
        ),
        (
            7,
            json!({"supported":true}),
            None,
            Some(ReasoningEffort::Minimal),
            None,
            Some("high"),
        ),
        (8, json!({}), None, None, None, None),
        (9, json!({}), None, None, None, Some("high")),
        (
            10,
            json!({"supported":true}),
            None,
            Some(ReasoningEffort::Medium),
            None,
            Some("high"),
        ),
    ] {
        let model = "claude-sonnet-4-6";
        let mut sdk:lingxi_llm_client::protocol::ProviderProfile = serde_json::from_value(json!({
            "provider_id":"anthropic","profile_name":"anthropic","base_url":"https://api.anthropic.com",
            "protocol":"anthropic_messages","auth":"none", "models":[{"display_model":model,"request_model":model,"billing_model":model}]
        })).unwrap();
        sdk.models[0].info.features.effort = EffortSupport {
            default,
            levels: (index == 8).then(Vec::new),
            ..Default::default()
        };
        let http = Arc::new(EffortDirectory(
            json!({"data":[{"id":model,"type":"model","capabilities":{"effort":effort}}],"has_more":false,"first_id":model,"last_id":model}),
        ));
        let (directory_client, config) =
            lingxi_llm_client::LlmClientBuilder::with_transport(http, &[sdk])
                .with_region(lingxi_llm_client::protocol::Region::International)
                .build_managed()
                .unwrap();
        let path = temp.join(index.to_string());
        config.set_config_dir(&path).await.unwrap();
        config
            .set_tracked_models("anthropic", [model.into()])
            .await
            .unwrap();
        config.sync_provider("anthropic", None).await.unwrap();
        let selected = directory_client
            .snapshot()
            .profile("anthropic")
            .unwrap()
            .clone();
        let mut row = profile(ProtocolFamily::AnthropicMessages, true);
        row.wire_profile = Some(selected);
        if index == 2 {
            row.models[0].display_model = "Custom Sonnet label".into();
        }
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![row],
            })
            .unwrap(),
        );
        let listing = client.available_models();
        if index == 0 || index == 8 {
            assert!(listing[0].reasoning.levels.is_empty());
        } else if index == 1 || index == 3 {
            assert!(!listing[0].reasoning.levels.iter().any(|id| id == "max"));
        } else if index == 2 {
            assert!(listing[0].reasoning.levels.iter().any(|id| id == "xhigh"));
        }
        let capture = Arc::new(Capture::default());
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        );
        service.set_session_effort(SessionEffort::Default);
        let mut request = LlmRequest::new(model).with_user_text("observed effort facts");
        if let Some(level) = primary {
            request.set_effort(Some(json!(level))).unwrap();
        }
        request.execution.effort_state.carried = carried.map(|level| json!(level));
        if index == 10 {
            request.execution.effort_state.catalog_default = Some(json!("high"));
        }
        service
            .execute_non_stream_request(
                request,
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default(),
            )
            .await
            .unwrap_err();
        let wires = capture.0.lock().unwrap();
        assert_eq!(wires.len(), 1);
        let body: Value = serde_json::from_slice(&wires[0].body).unwrap();
        assert_eq!(
            body.pointer("/output_config/effort")
                .and_then(Value::as_str),
            expected,
            "catalog case {index}"
        );
        let beta = wires[0]
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| value.as_str())
            .unwrap_or_default();
        assert_eq!(
            beta.split(',').any(|value| value == "effort-2025-11-24"),
            expected.is_some(),
            "catalog beta case {index}"
        );
    }
    std::fs::remove_dir_all(temp).unwrap();
}

async fn current_fast_catalog_reaches_transport() {
    for (model, supported) in [("claude-opus-4-7", false), ("claude-opus-5-5", true)] {
        for direct in [false, true] {
            let mut provider = profile(ProtocolFamily::AnthropicMessages, true);
            provider.models[0].display_model = model.into();
            provider.models[0].request_model = model.into();
            provider.models[0].billing_model = model.into();
            if !direct {
                provider.base_url = "https://fixture-gateway.example".into();
            }
            let client = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![provider],
                })
                .unwrap(),
            );
            let capture = Arc::new(Capture::default());
            let service = ApiService::new_with_routing(
                client,
                capture.clone(),
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
            .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
                cached_org_enabled: true,
                ..Default::default()
            }))
            .with_thinking(ThinkingConfig::Disabled);
            for stream in [false, true] {
                let mut request = LlmRequest::new(model).with_user_text("fast fixture");
                request.set_speed(Some("fast".into())).unwrap();
                if stream {
                    let _ = service.stream_request(request).await;
                } else {
                    let _ = service
                        .execute_non_stream_request(
                            request,
                            llm_runtime::NonStreamingRequestClass::Auxiliary,
                            llm_runtime::NonStreamingRetryOptions::default(),
                        )
                        .await;
                }
                let requests = capture.0.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    if stream { 2 } else { 1 },
                    "{model}, direct={direct}"
                );
                let wire = requests.last().unwrap();
                let body: Value = serde_json::from_slice(&wire.body).unwrap();
                assert_eq!(
                    body.get("speed").and_then(Value::as_str),
                    if direct && supported {
                        Some("fast")
                    } else {
                        None
                    }
                );
                let betas = wire
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or("");
                assert_eq!(betas.contains("fast-mode-2026-02-01"), direct && supported);
            }
        }
    }
}

async fn current_fast_environment_reaches_transport() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/fast_environment_2_1_287.json")).unwrap();
    let mut services = std::collections::HashMap::new();
    for row in fixture["cases"].as_array().unwrap() {
        let model = row["model"].as_str().unwrap();
        let canonical = row["canonical"].as_str().unwrap();
        let native_provider = row["provider"].as_str().unwrap();
        let key = (model.to_string(), native_provider.to_string());
        let (service, capture) = services.entry(key).or_insert_with(|| {
            let mut provider = profile(
                if native_provider == "bedrock" {
                    ProtocolFamily::BedrockClaude
                } else {
                    ProtocolFamily::AnthropicMessages
                },
                true,
            );
            // A picker label selects its canonical wire row. Admission must use
            // that resolved identity, rather than the label supplied by a host.
            provider.models[0].display_model = model.into();
            provider.models[0].request_model = canonical.into();
            provider.models[0].billing_model = canonical.into();
            if native_provider == "gateway" {
                provider.base_url = "https://fixture-gateway.example".into();
            }
            let client = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![provider],
                })
                .unwrap(),
            );
            let capture = Arc::new(Capture::default());
            let service = ApiService::new_with_routing(
                client,
                capture.clone(),
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
            .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
                cached_org_enabled: true,
                ..Default::default()
            }))
            .with_thinking(ThinkingConfig::Disabled);
            (service, capture)
        });
        for (name, field) in [
            (branding::MODEL_CAPABILITIES_ENV, "environment"),
            (branding::DISABLE_FAST_MODE_ENV, "disabled"),
        ] {
            if let Some(value) = row[field].as_str() {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
        let allowed = row["expected"].as_bool().unwrap();
        assert_eq!(
            service.fast_model_allowed(model, None).unwrap(),
            allowed,
            "{row}"
        );
        for stream in [false, true] {
            capture.0.lock().unwrap().clear();
            let mut request = LlmRequest::new(model).with_user_text("fast environment fixture");
            request.set_speed(Some("fast".into())).unwrap();
            if stream {
                let _ = service.stream_request(request).await;
            } else {
                let _ = service
                    .execute_non_stream_request(
                        request,
                        llm_runtime::NonStreamingRequestClass::Auxiliary,
                        Default::default(),
                    )
                    .await;
            }
            let requests = capture.0.lock().unwrap();
            assert_eq!(
                requests.len(),
                1,
                "one physical SDK capture, stream={stream}: {row}"
            );
            let wire = &requests[0];
            let body: Value = serde_json::from_slice(&wire.body).unwrap();
            assert_eq!(
                body.get("speed").and_then(Value::as_str),
                allowed.then_some("fast"),
                "stream={stream}: {row}"
            );
            let beta = wire
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            assert_eq!(
                beta.contains("fast-mode-2026-02-01"),
                allowed,
                "stream={stream}: {row}"
            );
        }
    }
    std::env::remove_var(branding::MODEL_CAPABILITIES_ENV);
    std::env::remove_var(branding::DISABLE_FAST_MODE_ENV);
}

async fn fast_environment_is_refreshed_for_physical_retry() {
    struct RetryCapture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
    #[async_trait]
    impl Transport for RetryCapture {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            let mut requests = self.0.lock().unwrap();
            requests.push(request);
            let first = requests.len() == 1;
            std::env::set_var(branding::DISABLE_FAST_MODE_ENV, "true");
            Ok(lingxi_llm_client::HttpResponse {
                status: if first { 503 } else { 400 },
                headers: vec![],
                body: serde_json::to_vec(&json!({"type":"error","error":{"type":if first {"overloaded_error"} else {"invalid_request_error"},"message":"fixture"}})).unwrap().into(),
            }
            .into())
        }
    }
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"speed":"fast"}"#);
    std::env::set_var(branding::MODEL_CAPABILITIES_ENV, "fast_mode");
    std::env::remove_var(branding::DISABLE_FAST_MODE_ENV);
    let mut provider = profile(ProtocolFamily::AnthropicMessages, true);
    provider.models[0].display_model = "claude-opus-4-7".into();
    provider.models[0].request_model = "claude-opus-4-7".into();
    provider.models[0].billing_model = "claude-opus-4-7".into();
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![provider],
        })
        .unwrap(),
    );
    let capture = Arc::new(RetryCapture(Mutex::new(vec![])));
    let service = ApiService::new_with_routing(
        client,
        capture.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(1),
        Some(0),
    )
    .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }))
    .with_thinking(ThinkingConfig::Disabled);
    let mut request = LlmRequest::new("claude-opus-4-7").with_user_text("retry fast fixture");
    request.set_speed(Some("fast".into())).unwrap();
    let _ = service
        .execute_non_stream_request(
            request,
            llm_runtime::NonStreamingRequestClass::Auxiliary,
            Default::default(),
        )
        .await;
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (index, wire) in requests.iter().enumerate() {
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(
            body.get("speed").and_then(Value::as_str),
            (index == 0).then_some("fast")
        );
        let beta = wire
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("");
        assert_eq!(beta.contains("fast-mode-2026-02-01"), index == 0);
    }
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var(branding::MODEL_CAPABILITIES_ENV);
    std::env::remove_var(branding::DISABLE_FAST_MODE_ENV);
}
