use super::*;

#[test]
fn api_service_interactivity_is_session_local() {
    let prior = lingxi_core::host::session_flags::is_non_interactive_session();
    lingxi_core::host::session_flags::set_non_interactive_session(false);
    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(ok_response_json()),
        request_id: None,
    });

    let headless = make_adapter(transport.clone()).with_interactive_session(false);
    let interactive = make_adapter(transport).with_interactive_session(true);

    assert!(!headless.interactive_session_for_test());
    assert!(interactive.interactive_session_for_test());
    lingxi_core::host::session_flags::set_non_interactive_session(prior);
}

#[test]
fn prewarm_eligibility_follows_the_selected_route_and_transport() {
    let transport = || FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let anthropic = make_adapter_for_protocol_with_websocket(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "anthropic",
        "model",
        transport(),
        false,
    );
    let http_responses = make_adapter_for_protocol_with_websocket(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAI,
        "https://api.openai.com",
        "openai",
        "model",
        transport(),
        false,
    );
    let websocket_responses = make_adapter_for_protocol_with_websocket(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAI,
        "https://api.openai.com",
        "openai",
        "model",
        transport(),
        true,
    );

    assert!(!anthropic.supports_responses_websocket_prewarm("model", Some("anthropic")));
    assert!(!http_responses.supports_responses_websocket_prewarm("model", Some("openai")));
    assert!(websocket_responses.supports_responses_websocket_prewarm("model", Some("openai")));
    assert!(websocket_responses.supports_responses_websocket_prewarm("model", None));
    assert!(!websocket_responses.supports_responses_websocket_prewarm("unknown", None));
    assert!(!websocket_responses.supports_responses_websocket_prewarm("model", Some("other")));
}

#[test]
fn native_mid_conversation_system_memoizes_per_service_and_raw_model() {
    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "CLAUDE_CODE_MODEL_CAPABILITIES",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
        "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
    ]);
    let model = "claude-sonnet-4-6";
    let profile = native_capability_profile(
        ProviderId::VertexClaude,
        "vertex",
        ProtocolFamily::VertexClaude,
        "https://us-central1-aiplatform.googleapis.com",
        model,
        vec!["sonnet-route-alias".to_owned()],
    );
    let first_host = native_capability_adapter(vec![profile.clone()]);

    // QTe's explicit false wins before the model-age floor and is memoized
    // by the raw requested model string in this host/profile.
    std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model);
    std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES", "");
    assert!(!first_host.supports_native_mid_conversation_system(model, Some("vertex")));

    // The Native hQ/ Ta result stays stable for the same host and raw key
    // after process configuration changes.
    std::env::set_var(
        "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
        "mid_conversation_system",
    );
    assert!(!first_host.supports_native_mid_conversation_system(model, Some("vertex")));

    // A new raw key that resolves to the same route gets its own QTe
    // lookup, and a new ApiService gets an independent host cache.
    std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", "sonnet-route-alias");
    assert!(
        first_host.supports_native_mid_conversation_system("sonnet-route-alias", Some("vertex"))
    );
    std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model);
    let second_host = native_capability_adapter(vec![profile]);
    assert!(second_host.supports_native_mid_conversation_system(model, Some("vertex")));
}

#[test]
fn native_mid_conversation_system_cache_keeps_provider_profiles_separate() {
    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "CLAUDE_CODE_MODEL_CAPABILITIES",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
        "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
    ]);
    let model = "claude-sonnet-4-6";
    std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model);
    std::env::set_var(
        "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
        "mid_conversation_system",
    );
    let adapter = native_capability_adapter(vec![
        native_capability_profile(
            ProviderId::OpenAI,
            "openai",
            ProtocolFamily::OpenAiChat,
            "https://api.openai.com/v1",
            model,
            Vec::new(),
        ),
        native_capability_profile(
            ProviderId::AnthropicFirstParty,
            "anthropic",
            ProtocolFamily::AnthropicMessages,
            "https://api.anthropic.com",
            model,
            Vec::new(),
        ),
        native_capability_profile(
            ProviderId::VertexClaude,
            "vertex",
            ProtocolFamily::VertexClaude,
            "https://us-central1-aiplatform.googleapis.com",
            model,
            Vec::new(),
        ),
    ]);

    // A non-Anthropic route returns before entering the cache. First-party
    // routes disallow QTe model-option overrides; Vertex allows them. The
    // shared raw model string must retain each route's actual result.
    assert!(!adapter.supports_native_mid_conversation_system(model, Some("openai")));
    assert!(!adapter.supports_native_mid_conversation_system(model, Some("anthropic")));
    assert!(adapter.supports_native_mid_conversation_system(model, Some("vertex")));

    let reverse_order = native_capability_adapter(vec![
        native_capability_profile(
            ProviderId::AnthropicFirstParty,
            "anthropic",
            ProtocolFamily::AnthropicMessages,
            "https://api.anthropic.com",
            model,
            Vec::new(),
        ),
        native_capability_profile(
            ProviderId::VertexClaude,
            "vertex",
            ProtocolFamily::VertexClaude,
            "https://us-central1-aiplatform.googleapis.com",
            model,
            Vec::new(),
        ),
    ]);
    assert!(reverse_order.supports_native_mid_conversation_system(model, Some("vertex")));
    assert!(!reverse_order.supports_native_mid_conversation_system(model, Some("anthropic")));
}

/// `last_request_id()` captures the selected provider's server-side request
/// id (origin = Server), uses a sent SDK client UUID when present (origin =
/// Client), and clears when neither exists. Host telemetry IDs are not
/// provider request IDs and must not appear in this persisted field.
#[test]
fn last_request_id_captures_request_id_header() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    assert_eq!(adapter.last_request_id(), None);
    assert_eq!(adapter.last_request_id_origin(), None);

    let mut h = std::collections::BTreeMap::new();
    h.insert("request-id".to_string(), "req_011abc".to_string());
    adapter.record_rate_limit_from_headers_for_route(
        &h,
        Some("client_uuid"),
        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
        "anthropic",
        None,
    );
    assert_eq!(adapter.last_request_id(), Some("req_011abc".to_string()));
    // Server header present → server origin (client id ignored).
    assert_eq!(
        adapter.last_request_id_origin(),
        Some(RequestIdOrigin::Server)
    );

    // `x-request-id` remains a provider response fallback header → server origin.
    let mut h2 = std::collections::BTreeMap::new();
    h2.insert("x-request-id".to_string(), "req_xfallback".to_string());
    adapter.record_rate_limit_from_headers_for_route(
        &h2,
        Some("client_uuid"),
        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
        "anthropic",
        None,
    );
    assert_eq!(adapter.last_request_id(), Some("req_xfallback".to_string()));
    assert_eq!(
        adapter.last_request_id_origin(),
        Some(RequestIdOrigin::Server)
    );

    // No server id header → use only a client ID that was actually sent.
    adapter.record_rate_limit_from_headers_for_route(
        &std::collections::BTreeMap::new(),
        Some("client_uuid"),
        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
        "anthropic",
        None,
    );
    assert_eq!(adapter.last_request_id(), Some("client_uuid".to_string()));
    assert_eq!(
        adapter.last_request_id_origin(),
        Some(RequestIdOrigin::Client)
    );

    // Neither a server header nor a sent SDK client ID → no old Host ID fallback.
    adapter.record_rate_limit_from_headers_for_route(
        &std::collections::BTreeMap::new(),
        None,
        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
        "anthropic",
        None,
    );
    assert_eq!(adapter.last_request_id(), None);
    assert_eq!(adapter.last_request_id_origin(), None);
}

#[test]
fn build_request_keeps_system_source_for_sdk_projection() {
    let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    // The main getSystemPrompt source vector keeps the banner separate
    // from its body. Custom --system-prompt input has a distinct role.
    let system =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("rest body here"),
            ],
            None,
            None,
        );
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            Some(&system),
            vec![text_user_msg("hello")],
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    // The Host keeps system source in its execution context. The SDK
    // projects provider blocks and cache markers after route selection.
    assert_eq!(
        prompt_source_elements(&req)
            .iter()
            .map(|text| text.display_text())
            .collect::<Vec<_>>(),
        vec![HEADER, "rest body here"]
    );
    assert!(req.input.system.is_empty());
    assert!(req.input.prompt_cache.breakpoints.is_empty());
}

#[tokio::test]
async fn cache_ttl_source_is_deferred_until_an_anthropic_attempt_even_after_rerouting() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let reads = Arc::new(AtomicUsize::new(0));
    let calls = reads.clone();
    let adapter = make_request_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )))
    .with_prompt_cache_ttl_settings_source(Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(lingxi_core::settings::SettingsError::SourceUnavailable(
            "cache TTL",
        ))
    }));
    let system = "portable system instructions";
    let (mut request, _) = adapter
        .build_main_message_request(
            MessagesCreateRequest::new(
                "gpt-5",
                Some("openai"),
                custom_system_prompt(Some(system)),
                vec![text_user_msg("hello")],
                Vec::new(),
            ),
            false,
        )
        .unwrap();
    let openai = adapter.client.prepare(&request).await.unwrap();
    assert_eq!(
        openai
            .provider_request
            .body_json
            .string_at("/instructions")
            .as_deref(),
        Some(system)
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    request.profile = Some("anthropic".into());
    request.input.model = "claude-sonnet-4-20250514".into();
    for _ in 0..2 {
        assert!(matches!(
            adapter.client.prepare(&request).await,
            Err(LlmError::SettingsSource { .. })
        ));
    }
    assert_eq!(reads.load(Ordering::SeqCst), 1);

    request.profile = Some("openai".into());
    request.input.model = "gpt-5".into();
    adapter.client.prepare(&request).await.unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn build_request_projects_sampled_cache_ttl_settings_to_wire() {
    use lingxi_core::settings::schema::{
        PromptCacheTtl as HostTtl, PromptCacheTtlSettings as HostSettings,
    };

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "DISABLE_PROMPT_CACHING",
        "FORCE_PROMPT_CACHING_5M",
        "CLAUDE_CODE_PROMPT_CACHE_TTL",
        "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
        "ENABLE_PROMPT_CACHING_1H",
        "ENABLE_PROMPT_CACHING_1H_BEDROCK",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "LINGXI_GLOBAL_CACHE_SCOPE",
    ]);
    let settings = Arc::new(std::sync::Mutex::new(HostSettings {
        main: Some(HostTtl::OneHour),
        subagent: Some(HostTtl::FiveMinutes),
    }));
    let source_settings = settings.clone();
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )))
    .with_prompt_cache_ttl_settings_source(Arc::new(move || Ok(*source_settings.lock().unwrap())));
    let system_text = format!("{HEADER}{SECTION_SEP}retained settings prompt");
    let system = custom_system_prompt(Some(&system_text));

    let main = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            system.as_ref(),
            vec![text_user_msg("main")],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Named("sdk"),
        )
        .expect("main request");
    fn system_cache_ttl(body: &serde_json::Value) -> &str {
        body["system"]
            .as_array()
            .expect("wire system array")
            .iter()
            .find_map(|block| block["cache_control"]["ttl"].as_str())
            .expect("wire system cache breakpoint")
    }
    let main_prepared = adapter.client.prepare(&main).await.expect("prepare main");
    let main_body = request_body_value(&main_prepared.provider_request.body_json);
    assert_eq!(system_cache_ttl(&main_body), "1h");

    *settings.lock().unwrap() = HostSettings {
        main: Some(HostTtl::FiveMinutes),
        subagent: Some(HostTtl::OneHour),
    };
    let main_retry = adapter.client.prepare(&main).await.expect("prepare retry");
    assert_eq!(
        system_cache_ttl(&request_body_value(&main_retry.provider_request.body_json)),
        "1h"
    );
    let subagent = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            system.as_ref(),
            vec![text_user_msg("subagent")],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Subagent,
        )
        .expect("subagent request");
    let subagent_prepared = adapter
        .client
        .prepare(&subagent)
        .await
        .expect("prepare subagent");
    let subagent_body = request_body_value(&subagent_prepared.provider_request.body_json);
    assert_eq!(system_cache_ttl(&subagent_body), "1h");
}

/// The `tool_reference` normalization branch is selected by the SESSION-scoped
/// tool-search gate (Claude Code `$U()`), NOT by whether the request's own
/// toolset carries a `ToolSearch` declaration. This is the compaction /
/// side-query shape: the request is assembled with an EMPTY toolset, so the
/// old code (`tools contains "ToolSearch"`) inferred disabled and emitted the
/// wrong placeholder. With the fix the branch follows the published session
/// flag, matching the oracle's `if(!$U())W=j6s(W);else W=xPy(W,a)`.
#[test]
fn build_request_tool_reference_branch_follows_session_gate_not_toolset() {
    // A user tool_result carrying a tool_reference to a tool absent from the
    // (empty) availability set, paired with its assistant tool_use so
    // `ensure_tool_result_pairing` is a strict no-op.
    let convo = || {
        vec![
            ConversationMessage::Assistant {
                per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: lingxi_core::types::ToolUseId::from("toolu_ref"),
                    name: "ToolSearch".to_string(),
                    input: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                        .unwrap(),
                    provider_id: Some("toolu_ref".to_string()),
                }],
                stop_reason: None,
                resumable: None,
            },
            ConversationMessage::User {
                api_message_override: None,
                interrupted_by_shutdown: None,
                refused_message_id: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: lingxi_core::types::ToolUseId::from("toolu_ref"),
                    content: String::new().into(),
                    is_error: Some(false),
                    provider_tool_use_id: Some("toolu_ref".to_string()),
                    content_blocks: Some(
                        lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                            &serde_json::json!([{
                            "type": "tool_reference",
                            "tool_name": "mcp__gone__write"
                                }]),
                        )
                        .expect("tool reference fixture is valid JSON"),
                    ),
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
            },
        ]
    };
    // Read the placeholder text the converted tool_result carries.
    let placeholder = |req: &LlmRequest| -> String {
        for m in &req.input.messages {
            for b in &m.content {
                if let lingxi_llm_client::protocol::ContentBlock::ToolResult { blocks, .. } = b {
                    if let Some(text) = blocks
                        .as_ref()
                        .and_then(|blocks| blocks.array_item(0))
                        .and_then(|item| item.string_at("/text"))
                    {
                        return text;
                    }
                }
            }
        }
        panic!("no tool_result placeholder found in {req:?}");
    };

    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);

    // Session gate ON, EMPTY toolset (no `ToolSearch` declaration present):
    // the ENABLED branch filters the unavailable reference against the empty
    // availability set and emits the "tools no longer available" placeholder.
    adapter.set_tool_search_enabled(true);
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            convo(),
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert_eq!(
        placeholder(&req),
        "[Tool references removed - tools no longer available]",
        "empty toolset in a tool-search-enabled session must take the enabled branch"
    );

    // A second session must not overwrite the first session's enabled gate.
    let peer = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    peer.set_tool_search_enabled(false);
    let peer_request = peer
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            convo(),
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    assert_eq!(
        placeholder(&peer_request),
        "[Tool references removed - tool search not enabled]"
    );
    let original_request = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            convo(),
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    assert_eq!(
        placeholder(&original_request),
        "[Tool references removed - tools no longer available]"
    );

    // Session gate OFF: the DISABLED branch strips every reference with the
    // "tool search not enabled" placeholder, regardless of the toolset.
    adapter.set_tool_search_enabled(false);
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            convo(),
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert_eq!(
        placeholder(&req),
        "[Tool references removed - tool search not enabled]",
        "a tool-search-disabled session must take the disabled branch"
    );
}

#[test]
fn thinking_is_provider_aware() {
    use crate::model::thinking::ThinkingConfig;
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport);
    let build = |model: &str| {
        adapter
            .build_request(
                model,
                None,
                None,
                vec![text_user_msg("hi")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request")
            .input
            .thinking
    };

    // Claude with the default (Adaptive) thinking → Adaptive, byte-faithful.
    assert_eq!(
        build("claude-opus-4-8-20260115"),
        Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
            ..Default::default()
        })
    );
    // Non-Claude with the default (Adaptive) → NO reasoning field (provider
    // applies its own default instead of a forced high-effort / budget-0).
    assert_eq!(build("gpt-5"), None);
    assert_eq!(build("gemini-2.5-pro"), None);
    assert_eq!(build("deepseek-reasoner"), None);

    // An EXPLICIT fixed budget on a non-Claude model is still honored.
    let adapter_fixed = make_request_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )))
    .with_thinking(ThinkingConfig::Enabled {
        budget_tokens: 4096,
    });
    let r = adapter_fixed
        .build_request(
            "gpt-5",
            None,
            None,
            vec![text_user_msg("hi")],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request")
        .input
        .thinking;
    assert_eq!(
        r,
        Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Enabled),
            budget: Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(4096)),
            effort: None
        })
    );
}

/// `None` must mean the model-aware request default, not a codec default.
///
/// This is what makes `messages_create_side_query`'s `Option<u32>` worth
/// having. A caller with no real reason to pick a number used to be forced
/// to invent a ceiling, and an invented ceiling is sized against the ANSWER
/// while the provider bills THINKING to the same number. The app
/// questionnaire stage picked 4096, and on `deepseek-v4-pro` a 3123-token
/// reasoning pass hit `finish_reason: length` at 4098, cutting the tool call
/// mid-JSON.
#[tokio::test]
async fn a_side_query_without_a_ceiling_uses_the_model_request_default() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let model = "claude-sonnet-4-20250514";
    let build = |max_tokens| {
        adapter
            .build_request(
                model,
                None,
                None,
                vec![text_user_msg("hello")],
                vec![],
                false,
                max_tokens,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request")
    };

    let model_default =
        u32::try_from(crate::model::context_window::default_output_tokens_for_model(model))
            .expect("the model default fits u32");
    let automatic = build(None);
    assert_eq!(automatic.input.max_tokens, None);
    let prepared = adapter
        .client
        .prepare(&automatic)
        .await
        .expect("SDK resolves the current model-aware application default");
    let default_raw = model_default.to_string();
    assert_eq!(
        prepared
            .provider_request
            .body_json
            .raw_source_at("/max_tokens"),
        Some(default_raw.clone()),
        "an absent ceiling reaches the wire as the current model-aware default"
    );
    assert!(
        model_default > 4096,
        "this model must have headroom above the figure the app stages \
         used to invent, or the test cannot tell the two apart"
    );
    let explicit = build(Some(4096));
    assert_eq!(
        explicit.input.max_tokens,
        Some(4096),
        "an explicit ceiling must still be honored verbatim"
    );
    let prepared = adapter
        .client
        .prepare(&explicit)
        .await
        .expect("the explicit ceiling prepares");
    assert_eq!(
        prepared
            .provider_request
            .body_json
            .raw_source_at("/max_tokens"),
        Some("4096".to_owned()),
        "an explicit ceiling remains unchanged on the wire"
    );
}

#[test]
fn build_request_keeps_system_source_when_cache_is_disabled() {
    let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("DISABLE_PROMPT_CACHING", "1");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let system =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("rest body here"),
            ],
            None,
            None,
        );
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            Some(&system),
            vec![text_user_msg("hello")],
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    assert_eq!(
        prompt_source_elements(&req)
            .iter()
            .map(|text| text.display_text())
            .collect::<Vec<_>>(),
        vec![HEADER, "rest body here"]
    );
    assert!(req.input.system.is_empty());
    assert!(req.input.prompt_cache.breakpoints.is_empty());
}

#[test]
fn build_request_global_cache_gate_respects_experimental_beta_disable() {
    // Native suppresses the source boundary when experimental betas are
    // disabled, independent of subscription or local opt-in state.
    use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY as SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
    let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");
    std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
    std::env::remove_var("ANTHROPIC_BASE_URL");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let sections = [
        lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
            text: "shared".into(),
            scope: lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Shared,
        },
        lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
            text: "session".into(),
            scope: lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Session,
        },
    ];
    assert_eq!(
        adapter.prompt_snapshot_source_vector("claude-sonnet-4-20250514", None, &sections),
        vec![
            PromptText::from_string("shared"),
            PromptText::from_string("session")
        ]
    );
    let system =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            Some(&system),
            vec![text_user_msg("hi")],
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    // The source vector and resolved kill switch stay in the request
    // context until SDK egress selects and projects the provider route.
    assert_eq!(
        prompt_source_elements(&req)
            .iter()
            .map(|text| text.display_text())
            .collect::<Vec<_>>(),
        vec![HEADER, "static", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "dynamic"]
    );
    assert!(
        req.execution
            .prompt_cache
            .as_ref()
            .expect("request retains the SDK prompt context")
            .policy
            .experimental_betas_disabled
    );
    assert!(req.input.system.is_empty());
    assert!(req.input.prompt_cache.breakpoints.is_empty());
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
}

#[test]
fn build_request_global_cache_gate_uses_native_default_without_subscription_or_opt_in() {
    // With an untainted host and Native's default beta state, the source
    // vector reaches SDK projection without a subscriber or local opt-in.
    use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY as SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
    let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
    std::env::remove_var("ANTHROPIC_BASE_URL");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let sections = [
        lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
            text: "shared".into(),
            scope: lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Shared,
        },
        lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
            text: "session".into(),
            scope: lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Session,
        },
    ];
    assert_eq!(
        adapter.prompt_snapshot_source_vector("claude-sonnet-4-20250514", None, &sections),
        vec![
            PromptText::from_string("shared"),
            PromptText::from_string(
                lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY,
            ),
            PromptText::from_string("session")
        ]
    );
    let system =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            Some(&system),
            vec![text_user_msg("hi")],
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert_eq!(
        prompt_source_elements(&req)
            .iter()
            .map(|text| text.display_text())
            .collect::<Vec<_>>(),
        vec![HEADER, "static", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "dynamic"]
    );
    assert!(
        !req.execution
            .prompt_cache
            .as_ref()
            .expect("request retains the SDK prompt context")
            .policy
            .experimental_betas_disabled
    );
    assert!(req.input.system.is_empty());
    assert!(req.input.prompt_cache.breakpoints.is_empty());
}

#[tokio::test]
async fn sdk_request_body_emits_native_global_scope_without_subscription_or_opt_in() {
    use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY;

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
    std::env::remove_var("ANTHROPIC_BASE_URL");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let system =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
    let request = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            Some(&system),
            vec![text_user_msg("hi")],
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("global-cache request");
    let prepared = adapter.client.prepare(&request).await.expect("SDK prepare");
    let body = request_body_value(&prepared.provider_request.body_json);
    assert_eq!(body["system"].as_array().unwrap().len(), 3);
    assert_eq!(body["system"][0]["text"], HEADER);
    assert_eq!(body["system"][1]["text"], "static");
    assert_eq!(body["system"][2]["text"], "dynamic");
    assert_eq!(body["system"][1]["cache_control"]["scope"], "global");
    assert!(!prepared
        .provider_request
        .body_json
        .as_json_source()
        .contains(DYNAMIC_BOUNDARY));
}

#[tokio::test]
async fn sdk_request_body_honors_native_process_base_url_gate() {
    use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY;

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
    std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.test");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let system =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
    let request = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            Some(&system),
            vec![text_user_msg("hi")],
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("proxy-gated request");
    let prepared = adapter.client.prepare(&request).await.expect("SDK prepare");
    let body = request_body_value(&prepared.provider_request.body_json);
    assert_eq!(body["system"].as_array().unwrap().len(), 2);
    assert_eq!(body["system"][0]["text"], HEADER);
    assert_eq!(body["system"][1]["text"], "static\n\ndynamic");
    assert!(body["system"][1].get("cache_control").is_some());
    assert_eq!(
        body["system"][1]
            .get("cache_control")
            .and_then(|value| value.get("scope")),
        None
    );
    assert!(!prepared
        .provider_request
        .body_json
        .as_json_source()
        .contains(DYNAMIC_BOUNDARY));
    std::env::remove_var("ANTHROPIC_BASE_URL");
}

#[test]
fn build_request_cache_editing_dormant_by_default() {
    // Gate OFF (default): no cache_reference on tool_results, no cache_edits
    // block — byte-identical to the pre-feature request.
    let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    std::env::remove_var("LINGXI_CACHE_EDITING");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    // Even a subscriber + injected edits must stay inert without the env.
    let adapter = make_adapter_with_subscriber(
        transport,
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    )
    .with_cache_editing_inputs(CacheEditingInputs {
        new_edits: vec![crate::CacheEdit::Delete {
            cache_reference: "toolu_zzz".to_string(),
        }],
        pinned: vec![],
    });
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            custom_system_prompt(Some("sys")).as_ref(),
            tool_result_conversation(),
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    // No cache_edits block anywhere.
    let encoded = serde_json::to_value(&req.input).unwrap().to_string();
    assert!(
        !encoded.contains("cache_edits"),
        "cache editing remains disabled"
    );
    assert!(
        !encoded.contains("cache_reference"),
        "cache references remain absent"
    );
}

#[test]
fn build_request_cache_editing_opt_in_remains_dormant_without_protocol() {
    // Even with the old opt-in env set, the path stays fail-closed until
    // the cache-editing beta/session latch and cross-call pinned state are
    // implemented together.
    use crate::CacheEdit;
    let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("DISABLE_PROMPT_CACHING");
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    std::env::set_var("LINGXI_CACHE_EDITING", "1");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    // pinned (pos 1, the tool_result user msg) deletes ref "dup" + "p1";
    // new (last user msg) deletes "dup" (collapsed by dedup) + "n1".
    let adapter = make_adapter_with_subscriber(
        transport,
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    )
    .with_cache_editing_inputs(CacheEditingInputs {
        new_edits: vec![
            CacheEdit::Delete {
                cache_reference: "dup".to_string(),
            },
            CacheEdit::Delete {
                cache_reference: "n1".to_string(),
            },
        ],
        pinned: vec![PinnedCacheEdits {
            user_message_index: 1,
            edits: vec![
                CacheEdit::Delete {
                    cache_reference: "dup".to_string(),
                },
                CacheEdit::Delete {
                    cache_reference: "p1".to_string(),
                },
            ],
        }],
    });
    let req = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            custom_system_prompt(Some("sys")).as_ref(),
            tool_result_conversation(),
            vec![],
            false,
            Some(1024),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    std::env::remove_var("LINGXI_CACHE_EDITING");

    // No cache_reference stamping and no cache_edits insertion anywhere.
    let encoded = serde_json::to_value(&req.input).unwrap().to_string();
    assert!(
        !encoded.contains("cache_edits"),
        "cache editing remains disabled"
    );
    assert!(
        !encoded.contains("cache_reference"),
        "cache references remain absent"
    );
}

#[test]
fn build_request_sets_profile_when_provided() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport);
    let req = adapter
        .build_request(
            "gpt-5.2",
            Some("github-copilot"),
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request with profile");
    assert_eq!(
        req.input.model, "gpt-5.2",
        "model must be preserved verbatim"
    );
    assert_eq!(
        req.profile.as_deref(),
        Some("github-copilot"),
        "profile must be threaded into LlmRequest"
    );
}

#[test]
fn build_request_leaves_profile_none_when_not_provided() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport);
    let req = adapter
        .build_request(
            "claude-opus-4-7",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request without profile");
    assert_eq!(
        req.input.model, "claude-opus-4-7",
        "model must be preserved verbatim"
    );
    assert!(
        req.profile.is_none(),
        "profile must be None when not passed"
    );
}

#[tokio::test]
async fn mod_request_effort_applies_only_inside_its_task_scope() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_request_adapter(transport);
    let ordinary = service
        .build_request(
            "claude-opus-4-7",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    let rewritten = with_mod_request_effort("low", async {
        service.build_request(
            "claude-opus-4-7",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
    })
    .await
    .unwrap();
    let side_query = with_mod_request_effort("low", async {
        service.build_side_query_request_with_thinking(
            "claude-opus-4-7",
            None,
            None,
            None,
            false,
            vec![],
            vec![],
            None,
            None,
            vec![],
            None,
            None,
            None,
            Some("mod_side_query"),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
        )
    })
    .await
    .unwrap();
    let again = service
        .build_request(
            "claude-opus-4-7",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    let effort = |request: &LlmRequest| {
        serde_json::to_value(request).unwrap()["input"]["thinking"]["effort"].clone()
    };
    assert_eq!(effort(&rewritten), serde_json::json!("low"));
    assert!(effort(&side_query).is_null());
    assert_eq!(effort(&ordinary), effort(&again));
}

#[test]
fn side_query_builder_preserves_all_canonical_request_controls() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let req = adapter
        .build_side_query_request_with_thinking(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            Some(
                &lingxi_core::types::utf16_json::Utf16JsonProjection::parse("\"system\"").unwrap(),
            ),
            None,
            false,
            vec![text_user_msg("payload")],
            vec![],
            Some(321),
            None,
            vec!["STOP".into()],
            None,
            Some(serde_json::json!("high")),
            Some(0.2),
            Some("fusion_analyst"),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
        )
        .expect("side-query request builder");
    assert_eq!(req.input.model, "claude-sonnet-4-20250514");
    assert_eq!(req.profile.as_deref(), Some("anthropic"));
    let expected_system_units = "system".encode_utf16().collect::<Vec<_>>();
    assert_eq!(
        prompt_source_elements(&req)[0].utf16_code_units(),
        expected_system_units.as_slice()
    );
    assert_eq!(req.input.messages.len(), 1);
    assert_eq!(req.input.max_tokens, Some(321));
    assert_eq!(req.input.stop_sequences, vec!["STOP"]);
    assert!(
        req.input
            .temperature
            .is_some_and(|temperature| temperature == 0.2_f32),
        "the f32 caller value must remain exact in canonical input"
    );
    assert_eq!(
        req.input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.effort),
        Some(lingxi_llm_client::protocol::ReasoningEffort::High)
    );
    assert_eq!(
        req.execution.query_source.as_deref(),
        Some("fusion_analyst")
    );
    assert!(req.execution.capture_retry_count);
}

#[test]
fn side_query_thinking_preserves_explicit_disabled_without_inventing_it() {
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));

    let mut disabled = LlmRequest::new("deepseek-flash");
    adapter.apply_side_query_thinking(
        &mut disabled,
        "deepseek-flash",
        Some(crate::model::thinking::ThinkingConfig::Disabled),
        None,
    );
    assert!(disabled.execution.side_thinking_disabled);
    assert_eq!(
        disabled
            .input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.mode),
        Some(lingxi_llm_client::protocol::ThinkingMode::Disabled)
    );

    let mut unspecified = LlmRequest::new("deepseek-flash");
    adapter.apply_side_query_thinking(&mut unspecified, "deepseek-flash", None, None);
    assert!(!unspecified.execution.side_thinking_disabled);
    assert!(unspecified.input.thinking.is_none());

    let mut enabled = LlmRequest::new("deepseek-flash");
    adapter.apply_side_query_thinking(
        &mut enabled,
        "deepseek-flash",
        Some(crate::model::thinking::ThinkingConfig::Enabled { budget_tokens: 512 }),
        None,
    );
    assert!(!enabled.execution.side_thinking_disabled);
    assert_eq!(
        enabled
            .input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.mode),
        Some(lingxi_llm_client::protocol::ThinkingMode::Enabled)
    );
    assert_eq!(
        enabled
            .input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget),
        Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(512))
    );
}

#[test]
fn reasoning_family_uses_selected_protocol_and_canonical_provider() {
    let service = reasoning_route_fixture();
    for (profile, family) in [
        ("office-sonnet", "anthropic"),
        ("company-key#1", "openai"),
        ("google-work", "gemini"),
        ("coding-west", "deepseek"),
        ("openai", "unknown-lab"),
    ] {
        assert_eq!(
            service
                .reasoning_provider_for_model("reasoning-model", Some(profile))
                .as_deref(),
            Some(family)
        );
    }
    assert!(service
        .reasoning_provider_for_model("missing", Some("openai"))
        .is_none());
}

#[test]
fn non_native_main_preserves_disabled_mode_and_explicit_request_effort() {
    let service =
        reasoning_route_fixture().with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    for profile in ["google-work", "coding-west", "company-key#1"] {
        let request = MessagesCreateRequest::new(
            "reasoning-model",
            Some(profile),
            None,
            Vec::new(),
            Vec::new(),
        );
        let (canonical, _) = service.build_main_message_request(request, false).unwrap();
        assert_eq!(
            canonical
                .input
                .thinking
                .as_ref()
                .and_then(|thinking| thinking.mode),
            Some(lingxi_llm_client::protocol::ThinkingMode::Disabled)
        );
    }
    let mut request = MessagesCreateRequest::new(
        "reasoning-model",
        Some("company-key#1"),
        None,
        Vec::new(),
        Vec::new(),
    );
    request.opts.effort = Some(serde_json::json!("high"));
    let (canonical, _) = service.build_main_message_request(request, false).unwrap();
    assert_eq!(
        canonical
            .input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.effort),
        Some(lingxi_llm_client::protocol::ReasoningEffort::High)
    );
}

#[test]
fn side_query_builder_preserves_source_system_utf16_units() {
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let source =
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""a\ud800b""#).unwrap();
    let request = adapter
        .build_side_query_request_with_thinking(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            Some(&source),
            None,
            false,
            vec![text_user_msg("payload")],
            vec![],
            Some(321),
            None,
            vec![],
            None,
            None,
            None,
            Some("permission_classifier"),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
        )
        .unwrap();
    assert_eq!(
        prompt_source_elements(&request)[0].utf16_code_units(),
        &[b'a' as u16, 0xd800, b'b' as u16]
    );
}

#[test]
fn side_query_builder_prefers_the_captured_provider_system_structure() {
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let captured =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
                    vec![b'a' as u16, 0xd800],
                ),
            ],
            Some(
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "dynamic",
                ),
            ),
            None,
        );
    let request = adapter
        .build_side_query_request_with_thinking(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
            Some(&captured),
            false,
            vec![text_user_msg("payload")],
            vec![],
            Some(321),
            None,
            vec![],
            None,
            None,
            None,
            Some("permission_classifier"),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
        )
        .unwrap();

    let elements = prompt_source_elements(&request);
    assert_eq!(elements.len(), 2);
    assert_eq!(elements[0].utf16_code_units(), &[b'a' as u16, 0xd800]);
    assert_eq!(
        elements[1].utf16_code_units(),
        "dynamic".encode_utf16().collect::<Vec<_>>()
    );
}

#[test]
fn side_query_builder_rejects_non_string_source_system() {
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null").unwrap();
    let error = adapter
        .build_side_query_request_with_thinking(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            Some(&source),
            None,
            false,
            vec![],
            vec![],
            Some(321),
            None,
            vec![],
            None,
            None,
            None,
            Some("permission_classifier"),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        LlmError::InvalidRequest { message }
            if message == "side-query system prompt must be a root JSON string"
    ));
}

#[test]
fn strict_side_query_builder_uses_json_schema_without_parent_tools() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"ok": {"type": "boolean"}},
        "required": ["ok"]
    });
    let req = adapter
        .build_json_schema_request_with_thinking(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            Some(&lingxi_core::types::utf16_json::Utf16JsonProjection::parse("\"judge\"").unwrap()),
            None,
            vec![text_user_msg("payload")],
            schema.clone(),
            Some(256),
            None,
            None,
            Some(0.0),
            Some("fusion_analyst"),
        )
        .expect("strict request builder");
    assert!(req.stream);
    assert!(req.input.tools.is_empty());
    assert!(req.input.tool_choice == lingxi_llm_client::protocol::ToolChoice::Auto);
    assert_eq!(req.input.max_tokens, Some(256));
    assert_eq!(
        req.input.output_format,
        lingxi_llm_client::protocol::OutputFormat::JsonSchema {
            name: "response".into(),
            schema: provider_payload(schema),
            strict: true
        }
    );
    assert_eq!(
        req.execution.query_source.as_deref(),
        Some("fusion_analyst")
    );
}

#[tokio::test]
async fn side_query_explicit_thinking_budget_survives_automatic_policy() {
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::GeminiGenerateContent,
        ProviderId::Gemini,
        "https://generativelanguage.googleapis.com",
    );
    let request = adapter
        .build_json_schema_request_with_thinking(
            "model",
            Some("p"),
            None,
            None,
            vec![text_user_msg("hello")],
            serde_json::json!({"type":"object","properties":{}}),
            Some(8192),
            Some(serde_json::json!(4096)),
            Some(crate::model::thinking::ThinkingConfig::Adaptive),
            None,
            Some("permission_classifier"),
        )
        .unwrap();
    assert!(!request.execution.automatic_thinking_budget);
    let prepared = adapter.client.prepare(&request).await.unwrap();
    let body = request_body_value(&prepared.provider_request.body_json);
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        4096
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
}

#[test]
fn build_request_adaptive_models_preserve_automatic_output_budget() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport); // default ThinkingConfig::Adaptive

    // Runtime preserves automatic intent; the SDK resolves each route budget.
    for model in [
        "claude-opus-4-6",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-fable-5-1",
        "claude-mythos-5-1",
    ] {
        let req = adapter
            .build_request(
                model,
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        assert_eq!(
            req.input
                .thinking
                .as_ref()
                .and_then(|thinking| thinking.mode),
            Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
            "{model} → adaptive"
        );
        assert!(
            req.input.temperature.is_none(),
            "{model} → no temperature when thinking on"
        );
        assert_eq!(
            req.input.max_tokens, None,
            "{model} → preserve automatic budget intent"
        );
    }
    clear_thinking_env();
}

#[test]
fn build_request_automatic_thinking_preserves_output_budget_intent() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport); // default Adaptive intent, but model disallows adaptive

    // Preserve the original automatic manual budget. SDK preparation
    // binds it to each selected attempt's actual output ceiling.
    let req = adapter
        .build_request(
            "claude-haiku-4-5",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert_eq!(req.input.max_tokens, None);
    assert_eq!(
        req.input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget),
        Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(63_999)),
        "preserve original thinking intent until SDK preparation"
    );
    assert!(req.execution.automatic_thinking_budget);
    assert!(
        req.input.temperature.is_none(),
        "thinking on → no temperature"
    );

    let mut side = req.clone();
    side.input.max_tokens = Some(8192);
    adapter.apply_side_query_thinking(
        &mut side,
        "claude-haiku-4-5",
        Some(crate::model::thinking::ThinkingConfig::Adaptive),
        None,
    );
    assert!(side.execution.automatic_thinking_budget);
    assert_eq!(side.input.thinking, req.input.thinking);
    assert_eq!(side.input.max_tokens, Some(8192));
    adapter.apply_side_query_thinking(
        &mut side,
        "claude-haiku-4-5",
        Some(crate::model::thinking::ThinkingConfig::Disabled),
        None,
    );
    assert!(!side.execution.automatic_thinking_budget);
    clear_thinking_env();
}

/// cc 2.1.198 "Subagents + compaction inherit extended thinking config" —
/// the SUBAGENT seam half. `ProviderApiAdapter`'s `agent::SubagentApiClient`
/// impl delegates 1:1 to `ApiService::messages_create` / `stream`, which
/// route every request through the SAME `build_request` and thus the SAME
/// session `self.thinking` (binary: the child session's options carry
/// `thinkingConfig: sDi(n.options.thinkingConfig, …)` @215628753). Lock
/// that the subagent entry point issues a wire body whose `thinking`
/// field matches the session config exactly like the main loop's.
#[tokio::test]
async fn subagent_entry_point_inherits_session_thinking_config() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let model = "claude-sonnet-4-20250514";

    // The shared main-loop builder preserves automatic intent; the issued
    // body fits that intent to this model's 32K application output budget.
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport.clone());
    let expected = match adapter
        .build_request(
            model,
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request")
        .input
        .thinking
    {
        Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
            ..
        }) => serde_json::json!({"type": "adaptive", "display": "updates"}),
        Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Enabled),
            budget: Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(budget_tokens)),
            ..
        }) => {
            assert_eq!(budget_tokens, 63_999, "original automatic intent");
            serde_json::json!({"type": "enabled", "budget_tokens": 31_999, "display": "updates"})
        }
        None => serde_json::Value::Null,
        other => panic!("unexpected thinking config: {other:?}"),
    };
    assert_ne!(
        expected,
        serde_json::Value::Null,
        "session thinking is ON by default"
    );
    adapter
        .messages_create(crate::MessagesCreateRequest::new(
            model,
            None,
            None,
            vec![],
            vec![],
        ))
        .await
        .expect("messages_create");
    let body = request_body_value(&transport.seen.lock().unwrap()[0].body_json);
    assert_eq!(body["max_tokens"], 32_000);
    assert_eq!(
        body.get("thinking")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        expected,
        "subagent seam body inherits the session thinking config"
    );

    // Explicit session config (fixed budget): the subagent seam carries it too.
    let transport2 = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter2 = make_adapter(transport2.clone()).with_thinking(
        crate::model::thinking::ThinkingConfig::Enabled {
            budget_tokens: 2_048,
        },
    );
    adapter2
        .messages_create(crate::MessagesCreateRequest::new(
            model,
            None,
            None,
            vec![],
            vec![],
        ))
        .await
        .expect("messages_create");
    let body2 = request_body_value(&transport2.seen.lock().unwrap()[0].body_json);
    assert_eq!(
        body2["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": 2_048, "display": "updates"}),
        "an explicit session budget rides on the subagent seam"
    );
    clear_thinking_env();
}

#[test]
fn live_thinking_update_applies_to_the_next_request() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter(transport);

    service.set_thinking(crate::model::thinking::ThinkingConfig::Enabled {
        budget_tokens: 4_096,
    });
    let enabled = service
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("enabled request");
    assert_eq!(
        enabled
            .input
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget),
        Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(4_096))
    );

    service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let disabled = service
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("disabled request");
    assert!(disabled.input.thinking.is_none());
    clear_thinking_env();
}

#[test]
fn build_request_disable_thinking_env_drops_reasoning_sets_temperature() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    std::env::set_var("LINGXI_DISABLE_THINKING", "1");
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport);

    let req = adapter
        .build_request(
            "claude-opus-4-8",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert!(
        req.input.thinking.is_none(),
        "thinking disabled → no reasoning"
    );
    // opus-4-8 is not in the `rhn` temperature-gate set → no temperature even
    // when thinking is env-disabled.
    assert!(
        req.input.temperature.is_none(),
        "opus-4-8 thinking-disabled → no temperature (not in rhn set)"
    );
    // max_tokens still uses the model limit even when thinking is disabled.
    assert_eq!(req.input.max_tokens, None);
    clear_thinking_env();
}

#[test]
fn build_request_explicit_max_tokens_override_wins() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport);
    // Escalation override (Some) honored verbatim.
    let req = adapter
        .build_request(
            "claude-opus-4-8",
            None,
            None,
            vec![],
            vec![],
            false,
            Some(7_777),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert_eq!(req.input.max_tokens, Some(7_777));
    clear_thinking_env();
}

#[test]
fn build_request_uses_selected_glm_ceiling_and_enforces_hard_output_limit() {
    let model = "z-ai/glm-5.2:free-build-request-test";
    let mut provider = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|provider| provider.profile_name == "openrouter")
        .unwrap();
    provider.wire_profile = None;
    provider.connection = Default::default();
    provider.models.truncate(1);
    let entry = &mut provider.models[0];
    entry.display_model = model.into();
    entry.request_model = model.into();
    entry.billing_model = model.into();
    entry.metadata.context_window_tokens = Some(256_000);
    entry.metadata.max_output_tokens = Some(230_400);
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let mut adapter = make_adapter(transport);
    adapter.client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![provider],
        })
        .unwrap(),
    );

    let ordinary = adapter
        .build_request(
            model,
            Some("openrouter"),
            None,
            vec![text_user_msg("hello")],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("ordinary GLM request");
    assert_eq!(ordinary.input.max_tokens, None);

    let oversized_override = adapter
        .build_request(
            model,
            Some("openrouter"),
            None,
            vec![text_user_msg("hello")],
            vec![],
            false,
            Some(u32::MAX),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("explicit GLM request");
    assert_eq!(oversized_override.input.max_tokens, Some(u32::MAX));
}

#[test]
fn build_request_preserves_explicit_limit_for_registered_custom_model() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport);
    let request = adapter
        .build_request(
            "custom-long-output-model",
            None,
            None,
            Vec::new(),
            Vec::new(),
            false,
            Some(150_000),
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("custom model request");

    assert_eq!(request.input.max_tokens, Some(150_000));
}

#[test]
fn build_request_catalog_routes_preserve_automatic_budget_intent() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let mut adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    adapter.client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: crate::builtin_presets().providers,
        })
        .unwrap(),
    );
    let mut checked = 0;
    for listing in adapter.client.available_models() {
        let limits = adapter
            .client
            .output_token_limits_in(&listing.display_model, Some(&listing.profile_name))
            .unwrap();
        if limits.max_output_tokens.is_none() {
            continue;
        }
        let req = adapter
            .build_request(
                &listing.display_model,
                Some(&listing.profile_name),
                None,
                vec![text_user_msg("hello")],
                vec![],
                true,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .unwrap();
        assert_eq!(
            req.input.max_tokens, None,
            "{}/{}",
            listing.profile_name, listing.request_model
        );
        checked += 1;
    }
    assert!(checked > 100, "checked {checked} routes");
    clear_thinking_env();
}

#[test]
fn build_request_thinking_disabled_config_drops_reasoning() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let mut adapter =
        make_adapter(transport).with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let mut config = adapter.client.configuration();
    let model = &mut config.providers[0].models[0];
    model.display_model = "claude-opus-4-8".into();
    model.request_model = model.display_model.clone();
    model.billing_model = model.display_model.clone();
    model.aliases.clear();
    adapter.client = Arc::new(adapter.client.reconfigured(config).unwrap());
    assert!(adapter.native_api_system_route("claude-opus-4-8", None));
    let req = adapter
        .build_request(
            "claude-opus-4-8",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert!(
        req.input.thinking.is_none(),
        "ThinkingConfig::Disabled → no reasoning"
    );
    // opus-4-8 is NOT in the `rhn` temperature-gate set → field omitted even
    // with thinking disabled (binary @205866168: `!xs && rhn(u) ? … : void 0`).
    assert!(
        req.input.temperature.is_none(),
        "opus-4-8 thinking-disabled → no temperature (not in rhn set)"
    );
    clear_thinking_env();
}

#[test]
fn build_request_thinking_disabled_temperature_only_for_rhn_models() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let mut adapter =
        make_adapter(transport).with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let mut config = adapter.client.configuration();
    let model = &mut config.providers[0].models[0];
    model.display_model = "claude-sonnet-4-5".into();
    model.request_model = model.display_model.clone();
    model.billing_model = model.display_model.clone();
    model.aliases.clear();
    adapter.client = Arc::new(adapter.client.reconfigured(config).unwrap());
    assert!(adapter.native_api_system_route("claude-sonnet-4-5", None));
    // claude-sonnet-4-5 IS in the `rhn` set → temperature:1 is sent.
    let req = adapter
        .build_request(
            "claude-sonnet-4-5",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert!(
        req.input.thinking.is_none(),
        "thinking disabled → no reasoning"
    );
    assert_eq!(
        req.input.temperature,
        Some(1.0),
        "sonnet-4-5 thinking-disabled → temperature:1 (in rhn set)"
    );
    clear_thinking_env();
}

#[test]
fn count_media_counts_top_level_images_across_messages() {
    let msgs = vec![user_with_imgs(0..3), user_with_imgs(3..5)];
    assert_eq!(count_media(&msgs), 5);
}

#[test]
fn tool_result_string_content_contributes_no_media() {
    let msgs = vec![ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![
            ContentBlock::ToolResult {
                tool_use_id: lingxi_core::types::ToolUseId::new(),
                content: "lots of text, no media".to_string().into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            },
            img(0),
        ],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }];
    assert_eq!(count_media(&msgs), 1);
}

#[test]
fn count_media_includes_nested_tool_result_media() {
    // An MCP image result populates `content_blocks` with `{"type":"image"}`
    // values; these MUST count toward the media cap (claude.ts:965-969), or an
    // image-heavy MCP transcript silently exceeds the API limit and 400s.
    let msgs = vec![ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id: lingxi_core::types::ToolUseId::new(),
            content: "see images".to_string().into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: Some(core_projection(&serde_json::json!([
                serde_json::json!({"type": "text", "text": "x"}),
                serde_json::json!({"type": "image", "source": {"data": "AAA"}}),
                serde_json::json!({"type": "image", "source": {"data": "BBB"}}),
            ]))),
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }];
    assert_eq!(count_media(&msgs), 2, "two nested image blocks must count");
}

#[test]
fn count_media_includes_nested_openai_image_url_media() {
    let msgs = vec![ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id: lingxi_core::types::ToolUseId::new(),
            content: "see image".to_string().into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: Some(core_projection(&serde_json::json!([{
                "type": "image_url",
                "image_url": {"url": "https://example.com/image.png"}
            }]))),
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }];
    assert_eq!(count_media(&msgs), 1);
}

#[test]
fn strip_excess_media_strips_nested_tool_result_media() {
    // Over the cap, nested tool_result media is stripped oldest-first
    // (claude.ts:982-999), leaving the text + the most-recent nested image.
    let msgs = vec![ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id: lingxi_core::types::ToolUseId::new(),
            content: "imgs".to_string().into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: Some(core_projection(&serde_json::json!([
                serde_json::json!({"type": "image", "source": {"data": "a"}}),
                serde_json::json!({"type": "image", "source": {"data": "b"}}),
                serde_json::json!({"type": "image", "source": {"data": "c"}}),
                serde_json::json!({"type": "text", "text": "keep"}),
            ]))),
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }];
    let stripped = strip_excess_media(msgs, 1);
    assert_eq!(
        count_media(&stripped),
        1,
        "nested media trimmed to the limit"
    );
    // The text block and the newest image survive.
    let ConversationMessage::User { content, .. } = &stripped[0] else {
        panic!("user message");
    };
    let ContentBlock::ToolResult {
        content_blocks: Some(blocks),
        ..
    } = &content[0]
    else {
        panic!("tool_result with content_blocks");
    };
    assert_eq!(
        blocks.array_len("").unwrap(),
        2,
        "one image + the text remain; got {blocks:?}"
    );
    assert!((0..blocks.array_len("").unwrap()).any(|index| {
        blocks
            .at_pointer(&format!("/{index}/type"))
            .and_then(lingxi_core::types::utf16_json::JsonValueView::as_str)
            == Some("text")
    }));
}

#[test]
fn strip_excess_media_trims_oldest_to_limit_without_touching_history() {
    let stored = vec![user_with_imgs(0..60), user_with_imgs(60..102)];
    assert_eq!(count_media(&stored), 102);

    let to_send = stored.clone();
    let trimmed = strip_excess_media(to_send, MAX_MEDIA_PER_REQUEST);

    assert_eq!(count_media(&trimmed), 100);
    let remaining = image_data_in_order(&trimmed);
    assert_eq!(remaining.len(), 100);
    assert_eq!(remaining.first().unwrap(), "img2");
    assert_eq!(remaining.last().unwrap(), "img101");
    assert!(!remaining.contains(&"img0".to_string()));
    assert!(!remaining.contains(&"img1".to_string()));

    assert_eq!(count_media(&stored), 102);
    assert_eq!(image_data_in_order(&stored).first().unwrap(), "img0");
}

#[test]
fn strip_excess_media_leaves_within_limit_messages_unchanged() {
    let msgs = vec![user_with_imgs(0..50), user_with_imgs(50..100)];
    let before = msgs.clone();
    let out = strip_excess_media(msgs, MAX_MEDIA_PER_REQUEST);
    assert_eq!(out, before, "exactly 100 media → no stripping");
    assert_eq!(count_media(&out), 100);
}

#[test]
fn strip_excess_media_no_images_is_noop() {
    let msgs = vec![ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "no media here".to_string(),
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }];
    let before = msgs.clone();
    let out = strip_excess_media(msgs, MAX_MEDIA_PER_REQUEST);
    assert_eq!(out, before);
}

/// Labels must be locked to api-client originals (Fix 2).
///
/// Ensures that re-naming a label here triggers a test failure so the
/// telemetry schema change is explicit.
#[test]
fn error_kind_labels_match_api_client_originals() {
    // api-client: Unauthorized → "unauthorized"
    assert_eq!(
        ApiService::error_kind(&LlmError::Authentication {
            message: String::new()
        }),
        "unauthorized"
    );
    assert_eq!(
        ApiService::error_kind(&LlmError::PermissionDenied {
            message: String::new()
        }),
        "unauthorized"
    );
    // api-client: Server → "server"
    assert_eq!(
        ApiService::error_kind(&LlmError::ProviderInternal),
        "server"
    );
    // api-client: Http → "http"
    assert_eq!(
        ApiService::error_kind(&LlmError::Transport {
            message: "t".into()
        }),
        "http"
    );
    // api-client: MalformedStream → "malformed_stream"
    assert_eq!(
        ApiService::error_kind(&LlmError::StreamInterrupted {
            message: "s".into()
        }),
        "malformed_stream"
    );
    // api-client: Overloaded → "overloaded"
    assert_eq!(
        ApiService::error_kind(&LlmError::Overloaded { repeated: false }),
        "overloaded"
    );
    // api-client: RateLimited → "rate_limited"
    assert_eq!(
        ApiService::error_kind(&LlmError::RateLimited {
            retry_after: None,
            scope: None
        }),
        "rate_limited"
    );
    // api-client: PromptTooLong → "prompt_too_long"
    assert_eq!(
        ApiService::error_kind(&LlmError::ContextOverflow { token_gap: 0 }),
        "prompt_too_long"
    );
    // 2.1.212 413 split: RequestTooLarge → "request_too_large" (distinct
    // from the context-window "prompt_too_long").
    assert_eq!(
        ApiService::error_kind(&LlmError::RequestTooLarge),
        "request_too_large"
    );
}

/// Task 8 gate: subscriber non-enterprise 429 → terminal immediately (no retry).
///
/// Wire: `SubscriberState { is_subscriber: true, is_enterprise: false }` +
/// a transport that always returns 429 → the adapter returns an error without
/// making a second request.
#[tokio::test]
async fn subscriber_429_is_terminal_in_adapter() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 429,
        headers: BTreeMap::new(),
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "You have reached your usage limit"}
        })),
        request_id: None,
    });
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(result.is_err(), "subscriber 429 must be terminal");
    // Only ONE execution — no retries.
    assert_eq!(
        transport.seen_count(),
        1,
        "subscriber 429 must not retry (seen_count should be 1)"
    );
}

#[tokio::test]
async fn claude_subscription_does_not_stop_api_key_rate_limit_retry() {
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse {
            status: 429,
            headers: BTreeMap::from([("retry-after".into(), "0".into())]),
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        }),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let mut service = make_adapter_with_retries(transport.clone(), Some(1));
    service.subscriber = SubscriberState {
        is_subscriber: true,
        is_enterprise: false,
    };
    service
        .messages_create(MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(transport.seen_count(), 2);
}

#[tokio::test]
async fn unknown_routes_return_the_routing_error_without_dispatch() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter(transport.clone());
    for (model, profile) in [
        ("unregistered-model", None),
        ("claude-sonnet-4-20250514", Some("unregistered-profile")),
    ] {
        let expected = service.protocol_for_model(model, profile).unwrap_err();
        let complete = service
            .messages_create(MessagesCreateRequest::new(
                model,
                profile,
                None,
                vec![text_user_msg("hi")],
                Vec::new(),
            ))
            .await
            .expect_err("the unknown route cannot execute");
        let stream = service
            .stream(
                model,
                profile,
                None,
                vec![text_user_msg("hi")],
                Vec::new(),
                None,
                None,
            )
            .await
            .err()
            .expect("the unknown route cannot open a stream");
        assert_eq!(complete, expected);
        assert_eq!(stream, expected);
    }
    assert_eq!(transport.seen_count(), 0);
}

#[test]
fn a_claude_shaped_name_on_an_openai_profile_uses_the_openai_controls() {
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAI,
        "https://api.openai.com/v1",
        "responses",
        "claude-sonnet-4-6",
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
    );
    let make = || {
        MessagesCreateRequest::new(
            "claude-sonnet-4-6",
            Some("responses"),
            None,
            vec![text_user_msg("hi")],
            Vec::new(),
        )
    };
    let (request, _) = service.build_main_message_request(make(), false).unwrap();
    assert!(request.input.thinking.is_none());

    service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let (request, _) = service.build_main_message_request(make(), false).unwrap();
    assert_eq!(
        request.input.thinking.and_then(|thinking| thinking.mode),
        Some(lingxi_llm_client::protocol::ThinkingMode::Disabled),
    );
    assert!(request.input.temperature.is_none());
}

#[tokio::test]
async fn a_cross_protocol_retry_keeps_portable_effort_and_omits_claude_defaults() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service =
        make_request_adapter(transport.clone()).with_request_metadata(crate::RequestMetadata {
            user_id: r#"{"session_id":"local-host"}"#.into(),
        });
    let (mut request, _) = service
        .build_main_message_request(
            MessagesCreateRequest::new(
                "claude-opus-4-6",
                Some("anthropic"),
                None,
                vec![text_user_msg("retry on other route")],
                Vec::new(),
            ),
            false,
        )
        .unwrap();
    assert!(request.execution.automatic_thinking_budget);
    assert!(request.input.thinking.is_some());
    assert!(request.input.metadata.string_at("/user_id").is_some());

    request.profile = Some("openai".into());
    request.input.model = "gpt-5".into();
    for effort in [None, Some(serde_json::json!("medium"))] {
        let mut next = request.clone();
        next.set_effort(effort).unwrap();
        let _ = service
            .execute_non_stream_request(next, NonStreamingRequestClass::Main, Default::default())
            .await;
    }

    assert_eq!(transport.seen_count(), 2);
    let default = transport.seen_body(0);
    assert!(default.payload_at("/reasoning").is_none());
    let explicit = transport.seen_body(1);
    assert_eq!(
        explicit.string_at("/reasoning/effort").as_deref(),
        Some("medium")
    );
    for body in [default, explicit] {
        assert!(body.payload_at("/metadata/user_id").is_none());
        assert!(body.payload_at("/thinking").is_none());
    }
}
