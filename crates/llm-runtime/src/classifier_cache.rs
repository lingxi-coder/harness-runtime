//! Authorization cache projection at the selected-route boundary. Positions
//! refer to full logical messages; SDK continuation runs AFTER this projection.
//! Reuse SDK validators instead of keeping a second model/region allowlist.
use crate::ClassifierCacheLayout;
use lingxi_llm_client::{
    codecs::{CodecContext, WireCodec},
    protocol::{
        AuthStrategy, CacheBreakpoint, CachePosition, CacheTtl, ChatRequest, OpenAiPromptCacheMode,
        OpenAiPromptCacheOptions, OpenAiPromptCacheTtl, PromptCachePolicy, ProtocolFamily,
        ProviderProfile,
    },
    RequestMode,
};

pub(crate) fn project(
    input: &mut ChatRequest,
    profile: &ProviderProfile,
    wire_model: &str,
    auth: AuthStrategy,
    mode: RequestMode,
    layout: &ClassifierCacheLayout,
    parent_session: Option<&str>,
    enabled: bool,
) {
    if !enabled {
        input.prompt_cache = PromptCachePolicy::default();
        return;
    }
    let context = CodecContext::new(profile, wire_model, mode);
    let claude: Option<&dyn WireCodec> = match profile.protocol {
        ProtocolFamily::AnthropicMessages => {
            Some(&lingxi_llm_client::codecs::anthropic::AnthropicMessagesCodec)
        }
        ProtocolFamily::BedrockClaude => Some(&lingxi_llm_client::hosting::BedrockClaudeCodec),
        ProtocolFamily::FoundryClaude => Some(&lingxi_llm_client::hosting::FoundryClaudeCodec),
        ProtocolFamily::VertexClaude => Some(&lingxi_llm_client::hosting::VertexClaudeCodec),
        _ => None,
    };
    if let Some(codec) = claude {
        // Keep the selected Native/cloud TTL and scope policy. Restore the
        // logical message boundaries if initial conversion used another wire.
        let mut probe = input.clone();
        if !probe.prompt_cache.breakpoints.is_empty() {
            for position in [layout.history, layout.action].into_iter().flatten() {
                if !probe
                    .prompt_cache
                    .breakpoints
                    .iter()
                    .any(|point| point.position == position)
                {
                    probe.prompt_cache.breakpoints.push(CacheBreakpoint {
                        position,
                        ttl: CacheTtl::FiveMinutes,
                        scope: None,
                    });
                }
            }
            probe
                .prompt_cache
                .breakpoints
                .sort_by_key(|point| point.position);
        }
        if codec.validate_request(&probe, &context).is_ok() {
            input.prompt_cache = probe.prompt_cache;
            return;
        }
        // Compatible providers can reject Native TTL/scope/marker vocabulary.
        // Degrade ONLY our generated hints, never the authorization request.
        for point in &mut probe.prompt_cache.breakpoints {
            point.ttl = CacheTtl::FiveMinutes;
            point.scope = None;
        }
        probe.prompt_cache.automatic = None;
        if codec.validate_request(&probe, &context).is_ok() {
            input.prompt_cache = probe.prompt_cache;
            return;
        }
        probe.prompt_cache = PromptCachePolicy {
            automatic: Some(CacheTtl::FiveMinutes),
            ..Default::default()
        };
        input.prompt_cache = if codec.validate_request(&probe, &context).is_ok() {
            probe.prompt_cache
        } else {
            PromptCachePolicy::default()
        };
        return;
    }
    input.prompt_cache = PromptCachePolicy::default();
    let openai = profile.provider_id.as_str() == "openai"
        && profile.protocol == ProtocolFamily::OpenAiResponses
        && matches!(auth, AuthStrategy::ApiKey | AuthStrategy::Bearer);
    let chat = matches!(profile.provider_id.as_str(), "qwen" | "openrouter")
        && profile.protocol == ProtocolFamily::OpenAiChat;
    if !openai && !chat {
        return;
    }
    let codec: &dyn WireCodec = if openai {
        &lingxi_llm_client::codecs::openai::responses::OpenAiResponsesCodec
    } else {
        &lingxi_llm_client::codecs::openai::chat::OpenAiChatCodec
    };
    // A one-point route caches the stable history first; action is the
    // fallback when no history exists. System is already in those prefixes.
    let mut positions = Vec::new();
    for point in [
        layout.history,
        layout.action,
        input
            .system
            .iter()
            .rposition(|block| !block.is_empty())
            .map(|index| CachePosition::System { index }),
    ]
    .into_iter()
    .flatten()
    {
        if !positions.contains(&point) {
            positions.push(point);
        }
    }
    let mut probe = input.clone();
    for ttl in [CacheTtl::ThirtyMinutes, CacheTtl::FiveMinutes] {
        // OpenRouter GPT uses the implicit final boundary in addition to our
        // three explicit ones; Anthropic routes also allow automatic caching.
        // Qwen and Alibaba reject automatic controls in the same SDK validator.
        for automatic in [true, false] {
            if automatic && profile.provider_id.as_str() != "openrouter" {
                continue;
            }
            for count in (1..=positions.len()).rev() {
                let mut policy = PromptCachePolicy {
                    automatic: automatic.then_some(ttl),
                    breakpoints: positions[..count]
                        .iter()
                        .map(|&position| CacheBreakpoint {
                            position,
                            ttl,
                            scope: None,
                        })
                        .collect(),
                    ..Default::default()
                };
                policy.breakpoints.sort_by_key(|point| point.position);
                if openai {
                    policy.prompt_cache_options = Some(OpenAiPromptCacheOptions {
                        mode: Some(OpenAiPromptCacheMode::Implicit),
                        ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
                    });
                    policy.prompt_cache_key = parent_session.map(|id| format!("tool-auth:{id}"));
                }
                probe.prompt_cache = policy;
                // No network, credential reads or retries. Invalid route, TTL,
                // regional model or breakpoint shape simply receives no new hint.
                if codec.validate_request(&probe, &context).is_ok() {
                    input.prompt_cache = probe.prompt_cache;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_llm_client::codecs::EncodeRequest;
    use lingxi_llm_client::protocol::{ContentBlock, ConversationMessage, SystemBlock};
    use serde_json::{json, Value};

    fn profile(provider: &str, protocol: &str, base: &str, model: &str) -> ProviderProfile {
        serde_json::from_value(json!({
            "provider_id":provider, "profile_name":"auth", "protocol":protocol,
            "base_url":base, "auth":"none", "models":[{
                "display_model":"alias", "request_model":model, "billing_model":model
            }]
        }))
        .unwrap()
    }
    fn input() -> ChatRequest {
        let mut request = ChatRequest::new("alias");
        request.system.push(SystemBlock::new("trusted rules"));
        request
            .messages
            .push(ConversationMessage::user_text("configuration"));
        let mut transcript = ConversationMessage::user_text("<transcript>");
        for text in ["history", "current action", "</transcript>", "fast suffix"] {
            transcript.content.push(ContentBlock::Text {
                text: text.into(),
                thought_signature: None,
                citations: None,
            });
        }
        request.messages.push(transcript);
        request
    }
    fn layout() -> ClassifierCacheLayout {
        ClassifierCacheLayout {
            logical_id: "judgment-1".into(),
            stage: "fast",
            history: Some(CachePosition::Message { index: 1, block: 1 }),
            action: Some(CachePosition::Message { index: 1, block: 2 }),
        }
    }
    fn apply(request: &mut ChatRequest, p: &ProviderProfile, layout: &ClassifierCacheLayout) {
        project(
            request,
            p,
            &p.models[0].request_model,
            AuthStrategy::ApiKey,
            RequestMode::Complete,
            layout,
            Some("parent"),
            true,
        );
    }
    fn wire(request: &ChatRequest, p: &ProviderProfile) -> Value {
        let context = CodecContext::new(p, &p.models[0].request_model, RequestMode::Complete);
        let codec: &dyn WireCodec = match p.protocol {
            ProtocolFamily::OpenAiResponses => {
                &lingxi_llm_client::codecs::openai::responses::OpenAiResponsesCodec
            }
            _ => &lingxi_llm_client::codecs::openai::chat::OpenAiChatCodec,
        };
        let encoded = codec
            .encode_request(EncodeRequest::new(request), &context)
            .unwrap();
        serde_json::from_slice(&encoded.body).unwrap()
    }

    #[test]
    fn provider_matrix_uses_sdk_route_model_ttl_and_point_limits() {
        for (provider, protocol, base, model, count, ttl) in [
            (
                "openai",
                "open_ai_responses",
                "https://api.openai.com/v1",
                "gpt-6.1-sol",
                3,
                CacheTtl::ThirtyMinutes,
            ),
            (
                "qwen",
                "open_ai_chat",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "qwen3.8-max",
                3,
                CacheTtl::FiveMinutes,
            ),
            (
                "openrouter",
                "open_ai_chat",
                "https://openrouter.ai/api/v1",
                "openai/gpt-6.1-sol",
                3,
                CacheTtl::ThirtyMinutes,
            ),
            (
                "openrouter",
                "open_ai_chat",
                "https://openrouter.ai/api/v1",
                "anthropic/claude-sonnet-4-6",
                3,
                CacheTtl::FiveMinutes,
            ),
            (
                "openrouter",
                "open_ai_chat",
                "https://openrouter.ai/api/v1",
                "qwen/qwen-plus",
                1,
                CacheTtl::FiveMinutes,
            ),
        ] {
            let p = profile(provider, protocol, base, model);
            let mut r = input();
            apply(&mut r, &p, &layout());
            assert_eq!(
                r.prompt_cache.breakpoints.len(),
                count,
                "{provider}/{model}"
            );
            assert!(r
                .prompt_cache
                .breakpoints
                .iter()
                .all(|point| point.ttl == ttl));
            if count == 1 {
                assert_eq!(
                    r.prompt_cache.breakpoints[0].position,
                    layout().history.unwrap()
                );
            }
            let encoded = wire(&r, &p);
            if provider == "openrouter" && model.starts_with("openai/") {
                assert_eq!(encoded["prompt_cache_options"]["mode"], "implicit");
            }
            if provider == "openai" {
                assert_eq!(encoded["prompt_cache_key"], "tool-auth:parent");
                assert_eq!(encoded["prompt_cache_options"]["mode"], "implicit");
                assert!(encoded.get("instructions").is_none());
                assert_eq!(encoded["input"][0]["role"], "developer");
                assert!(encoded["input"][0]["content"][0]
                    .get("prompt_cache_breakpoint")
                    .is_some());
                assert!(encoded["input"][2]["content"][1]
                    .get("prompt_cache_breakpoint")
                    .is_some());
                assert!(encoded["input"][2]["content"][2]
                    .get("prompt_cache_breakpoint")
                    .is_some());
            } else {
                assert!(
                    encoded["messages"][2]["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(
                            |part| part["text"] == "history" && part.get("cache_control").is_some()
                        ),
                    "{provider}/{model}: {encoded}"
                );
            }
        }
    }

    #[test]
    fn unsupported_endpoints_models_auth_and_provider_defaults_get_no_new_controls() {
        for (provider, protocol, base, model) in [
            (
                "openai",
                "open_ai_responses",
                "https://api.openai.com/v1",
                "gpt-5.4",
            ),
            (
                "openai",
                "open_ai_responses",
                "https://compatible.test/v1",
                "gpt-6.1-sol",
            ),
            (
                "openai",
                "open_ai_responses",
                "https://chatgpt.com/backend-api/codex",
                "gpt-6.1-sol",
            ),
            (
                "qwen",
                "open_ai_chat",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "unknown",
            ),
            (
                "qwen",
                "open_ai_chat",
                "https://compatible.test/v1",
                "qwen3.8-max",
            ),
            (
                "openrouter",
                "open_ai_chat",
                "https://openrouter.ai/api/v1",
                "unknown/model",
            ),
            (
                "deepseek",
                "open_ai_chat",
                "https://api.deepseek.com",
                "deepseek-chat",
            ),
            ("grok", "open_ai_chat", "https://api.x.ai/v1", "grok-4"),
            (
                "google",
                "gemini_generate_content",
                "https://generativelanguage.googleapis.com/v1beta",
                "gemini-3.8-flash",
            ),
        ] {
            let p = profile(provider, protocol, base, model);
            let mut r = input();
            apply(&mut r, &p, &layout());
            assert_eq!(
                r.prompt_cache,
                PromptCachePolicy::default(),
                "{provider}/{model}"
            );
        }
        let p = profile(
            "openai",
            "open_ai_responses",
            "https://api.openai.com/v1",
            "gpt-6.1-sol",
        );
        for auth in [
            AuthStrategy::ChatGptOAuth,
            AuthStrategy::OAuthBearer,
            AuthStrategy::ChatGptPlan,
        ] {
            let mut r = input();
            project(
                &mut r,
                &p,
                "gpt-6.1-sol",
                auth,
                RequestMode::Complete,
                &layout(),
                Some("parent"),
                true,
            );
            assert_eq!(r.prompt_cache, PromptCachePolicy::default());
        }
    }

    #[test]
    fn stage_change_keeps_prefix_and_single_point_without_history_targets_action() {
        let p = profile(
            "openai",
            "open_ai_responses",
            "https://api.openai.com/v1",
            "gpt-6.1-sol",
        );
        let mut fast = input();
        apply(&mut fast, &p, &layout());
        let mut thinking = fast.clone();
        thinking.messages[1].content[4] = ContentBlock::Text {
            text: "thinking suffix".into(),
            thought_signature: None,
            citations: None,
        };
        let mut stage = layout();
        stage.stage = "thinking";
        apply(&mut thinking, &p, &stage);
        let a = wire(&fast, &p);
        let b = wire(&thinking, &p);
        assert_eq!(a["input"][0], b["input"][0]);
        assert_eq!(
            &a["input"][2]["content"].as_array().unwrap()[..4],
            &b["input"][2]["content"].as_array().unwrap()[..4]
        );
        assert_eq!(fast.prompt_cache, thinking.prompt_cache);
        let p = profile(
            "openrouter",
            "open_ai_chat",
            "https://openrouter.ai/api/v1",
            "qwen/qwen-plus",
        );
        let mut r = input();
        stage.history = None;
        apply(&mut r, &p, &stage);
        assert_eq!(
            r.prompt_cache.breakpoints[0].position,
            stage.action.unwrap()
        );
    }

    #[test]
    fn route_change_reprojects_full_positions_without_mutating_messages() {
        let p = profile(
            "openai",
            "open_ai_responses",
            "https://api.openai.com/v1",
            "gpt-6.1-sol",
        );
        let mut r = input();
        let original = r.messages.clone();
        apply(&mut r, &p, &layout());
        let q = profile(
            "qwen",
            "open_ai_chat",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "qwen3.8-max",
        );
        apply(&mut r, &q, &layout());
        assert!(r.prompt_cache.prompt_cache_options.is_none());
        assert!(r.prompt_cache.prompt_cache_key.is_none());
        assert!(r
            .prompt_cache
            .breakpoints
            .iter()
            .all(|point| point.ttl == CacheTtl::FiveMinutes));
        project(
            &mut r,
            &p,
            "gpt-6.1-sol",
            AuthStrategy::ApiKey,
            RequestMode::Complete,
            &layout(),
            Some("parent"),
            false,
        );
        assert_eq!(r.prompt_cache, PromptCachePolicy::default());
        assert_eq!(r.messages, original);
    }
    #[test]
    fn compatible_claude_routes_degrade_only_generated_hints() {
        for (provider, extra, expected_points, expected_auto) in [
            ("minimax", json!({}), 4, None),
            ("kimi", json!({"explicit_cache":"unsupported"}), 0, None),
            (
                "kimi",
                json!({"explicit_cache":"top_level_only"}),
                0,
                Some(CacheTtl::FiveMinutes),
            ),
        ] {
            let mut p = profile(
                provider,
                "anthropic_messages",
                "https://compatible.test",
                "model",
            );
            p.extra = extra;
            let mut r = input();
            r.prompt_cache.breakpoints = vec![
                CacheBreakpoint {
                    position: CachePosition::System { index: 0 },
                    ttl: CacheTtl::OneHour,
                    scope: None,
                },
                CacheBreakpoint {
                    position: CachePosition::Message { index: 1, block: 4 },
                    ttl: CacheTtl::FiveMinutes,
                    scope: None,
                },
            ];
            let original = r.messages.clone();
            apply(&mut r, &p, &layout());
            assert_eq!(
                r.prompt_cache.breakpoints.len(),
                expected_points,
                "{provider}"
            );
            assert_eq!(r.prompt_cache.automatic, expected_auto, "{provider}");
            assert_eq!(r.messages, original);
            let context = CodecContext::new(&p, "model", RequestMode::Complete);
            lingxi_llm_client::codecs::anthropic::AnthropicMessagesCodec
                .validate_request(&r, &context)
                .unwrap();
        }
    }

    #[derive(Default)]
    struct Mock {
        bodies: std::sync::Mutex<Vec<Value>>,
        replies: std::sync::Mutex<std::collections::VecDeque<(u16, Value)>>,
    }
    impl Mock {
        fn reply(&self, status: u16, body: Value) {
            self.replies.lock().unwrap().push_back((status, body));
        }
        fn success(&self, id: &str) {
            self.reply(200, json!({"id":id,"status":"completed","model":"gpt-6.1-sol", "output":[{
                "type":"message","role":"assistant","content":[{"type":"output_text","text":"<block>yes","annotations":[]}]
            }],"usage":{"input_tokens":50,"output_tokens":3,"total_tokens":53}}));
        }
    }
    #[async_trait::async_trait]
    impl lingxi_llm_client::Transport for Mock {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            use futures::StreamExt;
            self.bodies
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&request.body).unwrap());
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected dispatch");
            Ok(lingxi_llm_client::StreamResponse {
                status,
                headers: vec![],
                body: futures::stream::once(async move {
                    Ok(bytes::Bytes::from(serde_json::to_vec(&body).unwrap()))
                })
                .boxed(),
            })
        }
    }
    #[tokio::test]
    async fn message_continuation_preserves_cache_policy_and_replays_full_input_once() {
        use lingxi_llm_client::{
            ConversationRequestMode, ConversationSession, LlmClientBuilder, RequestOptions,
        };
        let mut p = profile(
            "openai",
            "open_ai_responses",
            "https://api.openai.com/v1",
            "gpt-6.1-sol",
        );
        p.extra = json!({"supports_previous_response_id":true});
        let mock = std::sync::Arc::new(Mock::default());
        let client = LlmClientBuilder::with_transport(mock.clone(), &[p.clone()])
            .with_region(lingxi_llm_client::protocol::Region::International)
            .build()
            .unwrap();
        let mut session = ConversationSession::default();
        let options = RequestOptions {
            account_scope: Some("account-a".into()),
            ..Default::default()
        };
        let admitted = std::sync::atomic::AtomicUsize::new(0);
        let mut first = input();
        apply(&mut first, &p, &layout());
        mock.success("r1");
        let response = session
            .complete_in(
                &client.snapshot(),
                "auth",
                &first,
                &options,
                || {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                },
                |_| {},
            )
            .await
            .unwrap();
        let mut next = first.clone();
        next.messages.push(response.message);
        next.messages
            .push(ConversationMessage::user_text("next message"));
        apply(&mut next, &p, &layout());
        mock.reply(400, json!({"error":{"code":"previous_response_not_found", "message":"Previous response with id r1 not found."}}));
        mock.success("r2");
        session
            .complete_in(
                &client.snapshot(),
                "auth",
                &next,
                &options,
                || {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                },
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(admitted.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert_eq!(
            session.last_request().mode,
            ConversationRequestMode::Recreate
        );
        let bodies = mock.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[1]["previous_response_id"], "r1");
        assert_eq!(bodies[1]["input"].as_array().unwrap().len(), 1);
        assert!(bodies[2].get("previous_response_id").is_none());
        assert!(bodies[2]["input"].as_array().unwrap().len() > 1);
        assert_eq!(bodies[0]["input"][0], bodies[2]["input"][0]);
        assert_eq!(bodies[0]["input"][2], bodies[2]["input"][2]);
        assert_eq!(
            bodies[0]["prompt_cache_options"],
            bodies[2]["prompt_cache_options"]
        );
        assert_eq!(bodies[0]["prompt_cache_key"], bodies[2]["prompt_cache_key"]);
    }

    #[tokio::test]
    async fn replaced_classifier_suffix_and_switched_account_never_reuse_response_chain() {
        use lingxi_llm_client::{
            ConversationRequestMode, ConversationSession, LlmClientBuilder, RequestOptions,
        };
        let mut p = profile(
            "openai",
            "open_ai_responses",
            "https://api.openai.com/v1",
            "gpt-6.1-sol",
        );
        p.extra = json!({"supports_previous_response_id":true});
        let mock = std::sync::Arc::new(Mock::default());
        let client = LlmClientBuilder::with_transport(mock.clone(), &[p.clone()])
            .with_region(lingxi_llm_client::protocol::Region::International)
            .build()
            .unwrap();
        let mut session = ConversationSession::default();
        let mut options = RequestOptions {
            account_scope: Some("account-a".into()),
            ..Default::default()
        };
        let mut first = input();
        apply(&mut first, &p, &layout());
        mock.success("r1");
        session
            .complete_in(
                &client.snapshot(),
                "auth",
                &first,
                &options,
                || Ok(()),
                |_| {},
            )
            .await
            .unwrap();
        let mut thinking = input();
        thinking.messages[1].content[4] = ContentBlock::Text {
            text: "thinking suffix".into(),
            thought_signature: None,
            citations: None,
        };
        let mut stage = layout();
        stage.stage = "thinking";
        apply(&mut thinking, &p, &stage);
        mock.success("r2");
        let response = session
            .complete_in(
                &client.snapshot(),
                "auth",
                &thinking,
                &options,
                || Ok(()),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(session.last_request().mode, ConversationRequestMode::Full);
        thinking.messages.push(response.message);
        thinking
            .messages
            .push(ConversationMessage::user_text("followup"));
        options.account_scope = Some("account-b".into());
        mock.success("r3");
        session
            .complete_in(
                &client.snapshot(),
                "auth",
                &thinking,
                &options,
                || Ok(()),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(session.last_request().mode, ConversationRequestMode::Full);
        assert!(mock
            .bodies
            .lock()
            .unwrap()
            .iter()
            .all(|body| body.get("previous_response_id").is_none()));
    }
}
