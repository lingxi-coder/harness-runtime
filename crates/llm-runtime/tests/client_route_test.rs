//! Regression tests for client route test.

use llm_runtime::client::ModelRuntime;
use llm_runtime::{
    AuthStrategy, Capabilities, ClientConfig, CredentialConfig, LlmError, LlmRequest, ModelProfile,
    PricingConfig, ProtocolFamily, ProviderId, ProviderProfile, ProviderStreamTransport,
    ReasoningConfig,
};

#[tokio::test]
async fn client_builds_routes_from_config_and_lists_models() {
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "GPT-4o".to_string(),
                request_model: "gpt-4o".to_string(),
                billing_model: "gpt-4o".to_string(),
                aliases: vec!["fast".to_string()],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
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
        }],
    };

    let client = ModelRuntime::from_config(config).unwrap();
    assert_eq!(client.available_models().len(), 1);
    assert!(client.prepare(&LlmRequest::new("fast")).await.is_ok());
}

#[tokio::test]
async fn prepare_returns_route_identity_and_encodes_resolved_request_model() {
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "GPT-4o".to_string(),
                request_model: "gpt-4o".to_string(),
                billing_model: "gpt-4o".to_string(),
                aliases: vec!["fast".to_string()],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
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
        }],
    };

    let client = ModelRuntime::from_config(config).unwrap();
    let prepared = client.prepare(&LlmRequest::new("fast")).await.unwrap();

    assert_eq!(prepared.route.resolved_route.profile_name, "openai");
    assert_eq!(
        prepared.provider_request.url,
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(prepared.provider_request.body_json["model"], "gpt-4o");
}

#[tokio::test]
async fn provider_qualified_ui_ref_is_normalized_before_openai_compatible_encoding() {
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "deepseek".to_string(),
            },
            profile_name: "deepseek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "deepseek-flash".to_string(),
                request_model: "deepseek-flash".to_string(),
                billing_model: "deepseek-flash".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    reasoning: true,
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
        }],
    })
    .unwrap();

    for profile in [None, Some("deepseek")] {
        let mut request = LlmRequest::new("deepseek/deepseek-flash");
        if let Some(profile) = profile {
            request = request.with_profile(profile);
        }
        let prepared = client
            .prepare(&request)
            .await
            .expect("a known provider-qualified UI ref must remain routable");
        assert_eq!(prepared.route.resolved_route.profile_name, "deepseek");
        assert_eq!(
            prepared.provider_request.body_json["model"], "deepseek-flash",
            "only the provider-native model id may reach the wire"
        );
    }
}

#[tokio::test]
async fn slash_bearing_openrouter_wire_model_is_not_mistaken_for_a_ui_ref() {
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            profile_name: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "openrouter/auto".to_string(),
                request_model: "openrouter/auto".to_string(),
                billing_model: "openrouter/auto".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
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
        }],
    })
    .unwrap();

    let prepared = client
        .prepare(&LlmRequest::new("openrouter/auto").with_profile("openrouter"))
        .await
        .expect("a slash-bearing provider-native model id must remain intact");
    assert_eq!(
        prepared.provider_request.body_json["model"],
        "openrouter/auto"
    );
}

fn anthropic_fast_profile(profile_name: &str, base_url: &str) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: lingxi_llm_client::protocol::Region::all(),
        provider_id: ProviderId::AnthropicFirstParty,
        profile_name: profile_name.to_string(),
        base_url: base_url.to_string(),
        protocol: ProtocolFamily::AnthropicMessages,
        auth: AuthStrategy::ApiKey,
        credential: CredentialConfig::None,
        models: vec![ModelProfile {
            display_model: "claude-opus-5".to_string(),
            request_model: "claude-opus-5".to_string(),
            billing_model: "claude-opus-5".to_string(),
            aliases: vec![],
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                reasoning: true,
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

#[tokio::test]
async fn fast_speed_survives_only_on_the_builtin_anthropic_route() {
    let direct = ModelRuntime::from_config(ClientConfig {
        providers: vec![anthropic_fast_profile(
            "anthropic",
            "https://api.anthropic.com",
        )],
    })
    .unwrap();
    let mut request = LlmRequest::new("claude-opus-5");
    request.set_speed(Some("fast".to_string())).unwrap();
    let prepared = direct.prepare(&request).await.unwrap();
    assert_eq!(prepared.provider_request.body_json["speed"], "fast");

    let custom = ModelRuntime::from_config(ClientConfig {
        providers: vec![anthropic_fast_profile(
            "anthropic-compatible",
            "https://gateway.example",
        )],
    })
    .unwrap();
    let prepared = custom.prepare(&request).await.unwrap();
    assert!(
        prepared.provider_request.body_json.get("speed").is_none(),
        "custom Anthropic-compatible routes must not inherit first-party fast mode"
    );
}

#[tokio::test]
async fn fast_speed_is_removed_for_models_without_the_registry_capability() {
    let mut profile = anthropic_fast_profile("anthropic", "https://api.anthropic.com");
    profile.models[0].display_model = "claude-sonnet-5".to_string();
    profile.models[0].request_model = "claude-sonnet-5".to_string();
    profile.models[0].billing_model = "claude-sonnet-5".to_string();
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![profile],
    })
    .unwrap();
    let mut request = LlmRequest::new("claude-sonnet-5");
    request.set_speed(Some("fast".to_string())).unwrap();
    let prepared = client.prepare(&request).await.unwrap();
    assert!(prepared.provider_request.body_json.get("speed").is_none());
}

#[tokio::test]
async fn github_copilot_gpt5_and_codex_route_to_responses_endpoint() {
    // GitHub Copilot serves GPT-5.x / codex models ONLY via `/responses`, but
    // the provider profile declares one OpenAiChat protocol. The per-model
    // override must send those models to `…/responses` (same host) while older
    // models keep `/chat/completions`. Regression for the live-QA
    // "model gpt-5.5 is not accessible via the /chat/completions endpoint".
    let model = |display: &str, id: &str| ModelProfile {
        display_model: display.to_string(),
        request_model: id.to_string(),
        billing_model: id.to_string(),
        aliases: vec![],
        description: None,
        metadata: Default::default(),
        capabilities: Capabilities {
            streaming: true,
            tools: true,
            ..Default::default()
        },
    };
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "github-copilot".to_string(),
            },
            profile_name: "github-copilot".to_string(),
            base_url: "https://api.githubcopilot.com".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![
                model("GPT-5.5", "gpt-5.5"),
                model("GPT-5 Codex", "gpt-5-codex"),
                model("GPT-4o", "gpt-4o"),
            ],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    };
    let client = ModelRuntime::from_config(config).unwrap();

    for responses_model in ["gpt-5.5", "gpt-5-codex"] {
        let prepared = client
            .prepare(&LlmRequest::new(responses_model))
            .await
            .unwrap();
        assert_eq!(
            prepared.route.protocol,
            ProtocolFamily::OpenAiResponses,
            "{responses_model} must route via Responses"
        );
        assert_eq!(
            prepared.provider_request.url, "https://api.githubcopilot.com/responses",
            "{responses_model} must hit /responses"
        );
    }

    // Older models keep the chat/completions endpoint.
    let four = client.prepare(&LlmRequest::new("gpt-4o")).await.unwrap();
    assert_eq!(four.route.protocol, ProtocolFamily::OpenAiChat);
    assert_eq!(
        four.provider_request.url,
        "https://api.githubcopilot.com/chat/completions"
    );
}

#[tokio::test]
async fn non_copilot_openai_chat_provider_is_never_overridden() {
    // The override is scoped to the `github-copilot` profile: a GPT-5 id served
    // by some other OpenAiChat gateway (e.g. openrouter) keeps /chat/completions.
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            profile_name: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "GPT-5.5".to_string(),
                request_model: "gpt-5.5".to_string(),
                billing_model: "gpt-5.5".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
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
        }],
    };
    let client = ModelRuntime::from_config(config).unwrap();
    let prepared = client.prepare(&LlmRequest::new("gpt-5.5")).await.unwrap();
    assert_eq!(prepared.route.protocol, ProtocolFamily::OpenAiChat);
    assert_eq!(
        prepared.provider_request.url,
        "https://openrouter.ai/api/v1/chat/completions"
    );
}

#[tokio::test]
async fn reasoning_is_dropped_for_a_non_reasoning_model_not_hard_failed() {
    // Live-QA regression: with the session thinking config on, switching to a
    // model whose catalog capabilities advertise NO reasoning (e.g. an OpenRouter
    // free coder model) carried `request.reasoning`, and prepare hard-failed with
    // "unsupported capability: reasoning". Reasoning is a best-effort enhancement:
    // it must silently DROP for such a model, not break the turn.
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            profile_name: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "Qwen3 Coder (free)".to_string(),
                request_model: "qwen/qwen3-coder:free".to_string(),
                billing_model: "qwen/qwen3-coder:free".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    reasoning: false,
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
        }],
    };
    let client = ModelRuntime::from_config(config).unwrap();
    let mut req = LlmRequest::new("qwen/qwen3-coder:free");
    req.set_reasoning(Some(ReasoningConfig::Enabled {
        budget_tokens: 2048,
    }));

    // Must PREPARE OK (previously errored with UnsupportedCapability { reasoning }).
    let prepared = client
        .prepare(&req)
        .await
        .expect("reasoning must be dropped, not hard-fail the request");
    // And the reasoning must not leak onto the wire body.
    let body = prepared.provider_request.body_json.to_string();
    assert!(
        !body.contains("reasoning") && !body.contains("thinking"),
        "reasoning must be stripped from the wire body: {body}"
    );

    // The MID-CONVERSATION case: history carries Reasoning / RedactedThinking
    // blocks from an earlier thinking model. Those blocks must be stripped too —
    // `validate_capabilities` rejects them independently of the top-level field,
    // so a switch after any thinking must not hard-fail.
    let mut resumed = LlmRequest::new("qwen/qwen3-coder:free");
    assign_history(
        &mut resumed,
        &[
            llm_runtime::Message { api_output_config: None,
                role: "assistant".to_string(),
                content: vec![
                    llm_runtime::ContentBlock::Reasoning {
                        text: "let me think".to_string(),
                        signature: None,
                    },
                    llm_runtime::ContentBlock::Text {
                        text: "the answer is 42".to_string(),
                        cache_control: None,
                        citations: None,
                    },
                ],
            },
            llm_runtime::Message { api_output_config: None,
                role: "user".to_string(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "thanks".to_string(),
                    cache_control: None,
                    citations: None,
                }],
            },
        ],
    );
    // No top-level reasoning field this turn — only history blocks.
    let prepared = client
        .prepare(&resumed)
        .await
        .expect("history reasoning blocks must be stripped, not hard-fail");
    let body = prepared.provider_request.body_json.to_string();
    assert!(
        !body.contains("let me think"),
        "history reasoning block must be stripped from the wire body: {body}"
    );
    assert!(
        body.contains("the answer is 42"),
        "non-reasoning content must survive: {body}"
    );
}

#[tokio::test]
async fn vision_image_blocks_are_not_silently_dropped_for_non_vision_model() {
    // Regression guard: image-bearing requests without a vision delegate must
    // now fail explicitly instead of silently deleting media from the wire.
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            profile_name: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "Qwen3 Coder (free)".to_string(),
                request_model: "qwen/qwen3-coder:free".to_string(),
                billing_model: "qwen/qwen3-coder:free".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    vision: false,
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
        }],
    };
    let client = ModelRuntime::from_config(config).unwrap();

    // The common "paste image into input view" scenario.
    let mut image_request = LlmRequest::new("qwen/qwen3-coder:free");
    assign_history(
        &mut image_request,
        &[llm_runtime::Message { api_output_config: None,
            role: "user".to_string(),
            content: vec![
                llm_runtime::ContentBlock::Text {
                    text: "describe this image".to_string(),
                    cache_control: None,
                    citations: None,
                },
                llm_runtime::ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    bytes: vec![1, 2, 3],
                },
            ],
        }],
    );
    let error = client
        .prepare(&image_request)
        .await
        .expect_err("image block must fail explicitly without delegation");
    assert!(matches!(
        error,
        LlmError::UnsupportedCapability { capability } if capability == "vision"
    ));

    // The MID-CONVERSATION case is also explicit now: if history still carries
    // image blocks and no delegation layer rewrites them first, prepare must
    // surface the vision capability error rather than silently mutating the
    // request.
    let mut history_request = LlmRequest::new("qwen/qwen3-coder:free");
    assign_history(
        &mut history_request,
        &[
            llm_runtime::Message { api_output_config: None,
                role: "user".to_string(),
                content: vec![
                    llm_runtime::ContentBlock::Text {
                        text: "look at this".to_string(),
                        cache_control: None,
                        citations: None,
                    },
                    llm_runtime::ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        bytes: vec![1, 2, 3],
                    },
                ],
            },
            llm_runtime::Message { api_output_config: None,
                role: "assistant".to_string(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "i see a photo".to_string(),
                    cache_control: None,
                    citations: None,
                }],
            },
            llm_runtime::Message { api_output_config: None,
                role: "user".to_string(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "ok what else".to_string(),
                    cache_control: None,
                    citations: None,
                }],
            },
        ],
    );
    let error = client
        .prepare(&history_request)
        .await
        .expect_err("history image blocks must fail explicitly without delegation");
    assert!(matches!(
        error,
        LlmError::UnsupportedCapability { capability } if capability == "vision"
    ));
}

#[test]
fn duplicate_profile_names_are_rejected_during_client_construction() {
    let config = ClientConfig {
        providers: vec![
            ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::OpenAI,
                profile_name: "shared".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                protocol: ProtocolFamily::OpenAiChat,
                auth: AuthStrategy::Bearer,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: "GPT-4o".to_string(),
                    request_model: "gpt-4o".to_string(),
                    billing_model: "gpt-4o".to_string(),
                    aliases: vec!["fast".to_string()],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
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
            },
            ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "shared".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::Bearer,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: "Claude".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4-20250514".to_string(),
                    aliases: vec![],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
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
            },
        ],
    };

    let err = ModelRuntime::from_config(config).unwrap_err();
    assert!(matches!(err, LlmError::InvalidRequest { .. }));
}

/// `OpenAiResponses` profiles construct successfully (the old "no codec yet"
/// error is gone) and `prepare` targets `{base_url}/responses` with POST.
#[tokio::test]
async fn openai_responses_profile_prepares_post_to_responses_endpoint() {
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "openai-responses".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiResponses,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "gpt-4o".to_string(),
                request_model: "gpt-4o".to_string(),
                billing_model: "gpt-4o".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
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
        }],
    };

    // The old build_codec arm returned LlmError::InvalidRequest ("no codec
    // yet"); construction must now succeed.
    let client = ModelRuntime::from_config(config)
        .expect("OpenAiResponses must have a codec; the 'no codec yet' error is gone");

    let prepared = client.prepare(&LlmRequest::new("gpt-4o")).await.unwrap();
    assert_eq!(
        prepared.route.resolved_route.profile_name,
        "openai-responses"
    );
    assert_eq!(prepared.provider_request.method, "POST");
    assert!(
        prepared.provider_request.url.ends_with("/responses"),
        "URL must end with /responses; got: {}",
        prepared.provider_request.url
    );
    assert_eq!(
        prepared.provider_request.url,
        "https://api.openai.com/v1/responses"
    );
    assert_eq!(prepared.provider_request.body_json["model"], "gpt-4o");
}

#[tokio::test]
async fn openai_responses_websocket_capability_selects_stream_transport_only_for_streaming() {
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "openai-responses".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiResponses,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "gpt-5".to_string(),
                request_model: "gpt-5".to_string(),
                billing_model: "gpt-5".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: true,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: Some(1234),
            vision_delegate: None,
            connection: Default::default(),
        }],
    };
    let client = ModelRuntime::from_config(config).unwrap();

    let unary = client.prepare(&LlmRequest::new("gpt-5")).await.unwrap();
    assert_eq!(
        unary.provider_request.stream_transport,
        ProviderStreamTransport::Http
    );
    assert_eq!(unary.provider_request.websocket_connect_timeout_ms, None);

    let mut streaming_request = LlmRequest::new("gpt-5").with_user_text("hi");
    streaming_request.stream = true;
    let streaming = client.prepare(&streaming_request).await.unwrap();
    assert_eq!(
        streaming.provider_request.stream_transport,
        ProviderStreamTransport::ResponsesWebSocket
    );
    assert_eq!(
        streaming.provider_request.websocket_connect_timeout_ms,
        Some(1234)
    );
}

#[test]
fn websocket_capability_is_rejected_for_non_responses_protocols() {
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "openai-chat".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "gpt-4o".to_string(),
                request_model: "gpt-4o".to_string(),
                billing_model: "gpt-4o".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: true,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    };

    let err = ModelRuntime::from_config(config).unwrap_err();
    assert!(
        matches!(err, LlmError::InvalidRequest { ref message } if message.contains("OpenAiResponses")),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn response_format_is_rejected_when_selected_model_lacks_structured_output() {
    let config = ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "GPT-4o".to_string(),
                request_model: "gpt-4o".to_string(),
                billing_model: "gpt-4o".to_string(),
                aliases: vec!["fast".to_string()],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    structured_output: false,
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
        }],
    };

    let client = ModelRuntime::from_config(config).unwrap();
    let mut request = LlmRequest::new("fast");
    request.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonObject;

    let err = client.prepare(&request).await.unwrap_err();
    assert!(
        matches!(err, LlmError::UnsupportedCapability { capability } if capability == "structured_output")
    );
}

#[tokio::test]
async fn aliases_sharing_a_wire_id_keep_the_selected_rows_inference_contract() {
    use lingxi_llm_client::protocol::{CapabilitySupport, ReasoningEffort};
    let mut profile = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "openai")
        .unwrap();
    profile.auth = AuthStrategy::None;
    profile.credential = CredentialConfig::None;
    let model = profile
        .models
        .iter()
        .find(|m| m.request_model == "gpt-5.6-sol")
        .unwrap()
        .clone();
    let source = profile
        .wire_profile
        .as_ref()
        .unwrap()
        .models
        .iter()
        .find(|m| m.request_model == "gpt-5.6-sol")
        .unwrap()
        .clone();
    profile.models.clear();
    profile.wire_profile.as_mut().unwrap().models.clear();
    for (name, effort) in [
        ("low-row", ReasoningEffort::Low),
        ("high-row", ReasoningEffort::High),
    ] {
        let mut model = model.clone();
        model.display_model = name.into();
        model.aliases = vec![name.into()];
        let mut source = source.clone();
        source.display_model = name.into();
        source.aliases = vec![name.into()];
        source.info.features.effort.support = CapabilitySupport::Supported;
        source.info.features.effort.levels = Some(vec![effort]);
        profile.models.push(model);
        profile.wire_profile.as_mut().unwrap().models.push(source);
    }
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![profile],
    })
    .unwrap();
    let mut request = LlmRequest::new("low-row").with_user_text("hello");
    request.set_effort(Some(serde_json::json!("low"))).unwrap();
    let prepared = client.prepare(&request).await.unwrap();
    assert_eq!(prepared.provider_request.body_json["model"], "gpt-5.6-sol");
    assert_eq!(prepared.route.resolved_route.display_model, "low-row");
    request.input.model = "high-row".into();
    assert!(
        client.prepare(&request).await.is_err(),
        "a wire ID must not widen the selected effort levels"
    );
    request.set_effort(Some(serde_json::json!("high"))).unwrap();
    assert!(client.prepare(&request).await.is_ok());
}

fn assign_history(request: &mut llm_runtime::LlmRequest, messages: &[llm_runtime::Message]) {
    let (input, exact_strings) = llm_runtime::convert::history_input(
        &request.input.model,
        messages,
        &[],
        &[],
        lingxi_llm_client::protocol::ProtocolFamily::OpenAiChat,
    )
    .unwrap();
    request.input.messages = input.messages;
    request.input.prompt_cache = input.prompt_cache;
    request.execution.message_json_string_overrides = exact_strings;
}

#[tokio::test]
async fn native_hook_prompt_keeps_exact_input_until_sdk_final_serialization() {
    use lingxi_llm_client::protocol::{ContentBlock, ConversationMessage, MessageRole};

    let mut anthropic = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "anthropic")
        .expect("bundled Anthropic profile");
    anthropic.auth = AuthStrategy::None;
    anthropic.credential = CredentialConfig::None;
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![anthropic],
    })
    .unwrap();

    let mut request = LlmRequest::new("claude-sonnet-4-6");
    request.input.messages.push(ConversationMessage {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "\u{fffd}".into(),
            thought_signature: None,
            citations: None,
        }],
        native_options: Vec::new(),
    });
    request.execution.input_protocol = Some(ProtocolFamily::AnthropicMessages);
    request.execution.query_source = Some("hook_prompt".into());
    request
        .execution
        .message_json_string_overrides
        .insert("/messages/0/content/0/text".into(), vec![0xd800]);

    let prepared = client.prepare(&request).await.unwrap();
    let prompt_text = &prepared.provider_request.body_json["messages"][0]["content"][0]["text"];
    assert_eq!(prompt_text, "\u{fffd}");
    assert_eq!(
        prepared.provider_request.json_string_overrides["/messages/0/content/0/text"],
        vec![0xd800],
        "pre-serialization host view retains exact units"
    );
    let wire = prepared.provider_request.wire_body_bytes().unwrap();
    let wire = String::from_utf8(wire).unwrap();
    assert!(wire.contains("\u{fffd}"));
    assert!(!wire.contains("\\ud800"));

    let mut ordinary_request = request;
    ordinary_request.execution.query_source = None;
    let ordinary = client.prepare(&ordinary_request).await.unwrap();
    assert_eq!(
        ordinary.provider_request.json_string_overrides["/messages/0/content/0/text"],
        vec![0xd800]
    );
    let ordinary_wire = ordinary.provider_request.wire_body_bytes().unwrap();
    assert!(
        String::from_utf8(ordinary_wire)
            .unwrap()
            .contains("\\ud800")
    );
}

#[tokio::test]
async fn openai_codecs_reindex_and_retain_exact_text_units() {
    use lingxi_llm_client::protocol::{ContentBlock, ConversationMessage, MessageRole};

    for (protocol, encoded_pointer) in [
        (ProtocolFamily::OpenAiChat, "/messages/0/content"),
        (ProtocolFamily::OpenAiResponses, "/input/0/content/0/text"),
    ] {
        let mut openai = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "openai")
            .expect("bundled OpenAI profile");
        // Select the codec this control exercises explicitly. The current
        // bundled OpenAI route otherwise selects Responses, not Chat.
        openai.protocol = protocol;
        openai.auth = AuthStrategy::None;
        openai.credential = CredentialConfig::None;
        let model = openai
            .models
            .iter()
            .find(|model| model.request_model == "gpt-5.6-sol")
            .expect("OpenAI model")
            .request_model
            .clone();
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![openai],
        })
        .unwrap();

        let mut request = LlmRequest::new(model);
        request.input.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "display".into(),
                thought_signature: None,
                citations: None,
            }],
            native_options: Vec::new(),
        });
        request.execution.input_protocol = Some(ProtocolFamily::OpenAiChat);
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);

        let prepared = client.prepare(&request).await.unwrap();
        assert_eq!(prepared.route.protocol, protocol);
        assert_eq!(
            prepared.provider_request.json_string_overrides[encoded_pointer],
            vec![0xd800]
        );
        let wire = String::from_utf8(prepared.provider_request.wire_body_bytes().unwrap()).unwrap();
        assert!(wire.contains("\\ud800"));
        assert!(!wire.contains("\\ufffd"));
    }
}
