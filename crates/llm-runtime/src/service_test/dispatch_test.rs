use super::*;

#[test]
fn effective_subscriber_prefers_live_snapshot() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter_with_subscriber(transport, SubscriberState::default())
        .with_subscription(shared_slot(Some(
            lingxi_core::host::subscription::SubscriptionSnapshot {
                is_subscriber: true,
                subscription_type: Some("enterprise".to_string()),
                ..Default::default()
            },
        )));
    let sub = adapter.effective_subscriber();
    assert!(sub.is_subscriber);
    assert!(sub.is_enterprise);
}

#[test]
fn effective_subscriber_falls_back_when_slot_empty_or_absent() {
    let static_state = SubscriberState {
        is_subscriber: true,
        is_enterprise: false,
    };

    // No slot attached → static build-time state.
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter_with_subscriber(transport, static_state);
    let sub = adapter.effective_subscriber();
    assert!(sub.is_subscriber);
    assert!(!sub.is_enterprise);

    // Slot attached but unresolved (None) → static build-time state.
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter =
        make_adapter_with_subscriber(transport, static_state).with_subscription(shared_slot(None));
    let sub = adapter.effective_subscriber();
    assert!(sub.is_subscriber);
    assert!(!sub.is_enterprise);
}

#[test]
fn effective_subscriber_non_enterprise_tier_is_not_enterprise() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter_with_subscriber(transport, SubscriberState::default())
        .with_subscription(shared_slot(Some(
            lingxi_core::host::subscription::SubscriptionSnapshot {
                is_subscriber: true,
                subscription_type: Some("team".to_string()),
                ..Default::default()
            },
        )));
    let sub = adapter.effective_subscriber();
    assert!(sub.is_subscriber);
    assert!(!sub.is_enterprise);
}

#[tokio::test]
async fn bridge_resolves_and_forwards_local_model() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport.clone());
    let resp = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            custom_system_prompt(Some("sys")),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("ok");
    assert_eq!(resp.model, "claude-sonnet-4-20250514");
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
}

#[test]
fn available_models_non_empty() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let models = adapter.available_models();
    assert!(
        !models.is_empty(),
        "available_models must return at least one entry"
    );
}

#[tokio::test]
async fn bridge_forwards_batched_tools() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport.clone());
    let tools = vec![source_tool_declaration(
        "Read",
        "read a file",
        r#"{"type":"object"}"#,
    )];
    let _ = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            custom_system_prompt(Some("sys")),
            Vec::new(),
            tools.into_iter().map(Into::into).collect(),
        ))
        .await
        .expect("ok");
    assert_eq!(transport.seen_count(), 1);
}

#[tokio::test]
async fn messages_create_with_tools_on_tool_capable_model_succeeds() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport.clone());
    let tools = vec![source_tool_declaration(
        "Read",
        "read",
        r#"{"type":"object"}"#,
    )];
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            tools.into_iter().map(Into::into).collect(),
        ))
        .await;
    assert!(result.is_ok(), "tool-capable model should accept tools");
}

#[tokio::test]
async fn image_to_vision_model_is_allowed() {
    use lingxi_core::types::{ContentBlock, ConversationMessage, ImageSource, MessageId};
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let msgs = vec![ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://x/y.png".to_string(),
            },
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }];
    // The capability check is in ModelRuntime.validate_capabilities; since
    // FakeTransport doesn't inspect the body, this exercises the whole path.
    let _ = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            msgs,
            Vec::new(),
        ))
        .await;
}

/// Plan test: 429 with `retry-after` triggers one sleep then succeeds.
#[tokio::test]
async fn live_path_surfaces_rate_limit_headers() {
    let mut rate_limit_headers = BTreeMap::new();
    rate_limit_headers.insert("retry-after".to_string(), "1".to_string());

    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse {
            status: 429,
            headers: rate_limit_headers,
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        }),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter(transport.clone());
    let resp = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("ok after retry");
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    // Two executions: the 429 then the 200.
    assert_eq!(transport.seen_count(), 2);
}

/// Plan test: betas and User-Agent are injected post-prepare.
#[tokio::test]
async fn betas_and_user_agent_applied_post_prepare() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport.clone());
    let _ = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("ok");
    let headers = transport.seen_headers(0);
    assert!(
        headers.contains_key("anthropic-beta"),
        "anthropic-beta must be present; got: {headers:?}"
    );
    let ua = headers.get("user-agent").cloned().unwrap_or_default();
    assert!(
        ua.starts_with("claude-cli/"),
        "user-agent must start with claude-cli/; got: {ua}"
    );
}

#[tokio::test]
async fn dispatch_header_absent_by_default() {
    // Reader: asserts the DEFAULT (unflipped) value, so it only has to
    // exclude the flippers, not its fellow readers.
    let _g = cedar_lattice_read();
    let headers = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
    .await;
    assert!(
        !headers.contains_key("anthropic-dispatch-id"),
        "default-off: `tengu_cedar_lattice` defaults false — {headers:?}"
    );
}

/// `Mg` is `oMl({}, null)` (2.1.220 @226176806), so
/// `Mg.CLAUDE_CODE_DISPATCH_V2S` is ALWAYS `undefined` and the nullish `??`
/// always falls through to the flag: the env var can neither enable nor
/// disable the header in the oracle. Regression guard for the port having
/// treated a set env var as authoritative in both directions.
#[tokio::test]
async fn dispatch_header_ignores_the_inert_env_var() {
    let _g = cedar_lattice_write();
    std::env::set_var("CLAUDE_CODE_DISPATCH_V2S", "1");
    let flag_off = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
    .await;
    std::env::set_var("CLAUDE_CODE_DISPATCH_V2S", "0");
    let flag_on = {
        let _on = CedarLatticeOn::set();
        headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .await
    };
    std::env::remove_var("CLAUDE_CODE_DISPATCH_V2S");
    assert!(
        !flag_off.contains_key("anthropic-dispatch-id"),
        "a truthy env var must NOT enable what the oracle cannot enable"
    );
    assert_eq!(
        flag_on.get("anthropic-dispatch-id").map(String::as_str),
        Some("v2s"),
        "env \"0\" must NOT disable a true flag (`??` is nullish-only)"
    );
}

/// `Ooe() = xn()==="firstParty" && Yd()` (2.1.220 @227683488): BOTH halves
/// gate the header. `Yd()`/`d6r()`/`T1e()` require the base URL to resolve
/// to host `api.anthropic.com` unless
/// `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` is truthy.
#[tokio::test]
async fn dispatch_header_needs_both_halves_of_ooe() {
    let _g = cedar_lattice_write();
    let _on = CedarLatticeOn::set();
    let first_party = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
    .await;
    // Anthropic-wire but NOT first-party: fails the `xn()` half.
    let compatible = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::OpenAICompatible {
            name: "proxy".to_string(),
        },
        "https://proxy.example.com",
    )
    .await;
    // First-party id pointed at an enterprise gateway: fails the `Yd()`
    // half, which the port used not to model at all.
    let gateway_base = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.com",
    )
    .await;
    // `URL.host` keeps a non-default port, so this is NOT the allow-listed
    // host either.
    let ported_host = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com:8443",
    )
    .await;
    std::env::set_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1");
    let assumed = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.com",
    )
    .await;
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");

    assert_eq!(
        first_party.get("anthropic-dispatch-id").map(String::as_str),
        Some("v2s")
    );
    assert!(!compatible.contains_key("anthropic-dispatch-id"));
    assert!(
        !gateway_base.contains_key("anthropic-dispatch-id"),
        "Yd(): a custom API base must get no dispatch header"
    );
    assert!(!ported_host.contains_key("anthropic-dispatch-id"));
    assert_eq!(
        assumed.get("anthropic-dispatch-id").map(String::as_str),
        Some("v2s"),
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL short-circuits Yd()"
    );
}

/// `fB(i.querySource)!=="auxiliary"` (`fB` @227888239): utility queries —
/// the side-query driver's compaction / recap / title-generation traffic —
/// never carry the header, while an unclassified query (`fB(void 0)` ⇒
/// `undefined`) does.
#[tokio::test]
async fn dispatch_header_skips_auxiliary_queries() {
    let _g = cedar_lattice_write();
    let _on = CedarLatticeOn::set();
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    );
    let mut request = LlmRequest::new("model").with_user_text("hi");
    request.input.max_tokens = Some(4096);
    let mut aux = adapter.client.prepare(&request).await.expect("prepare");
    adapter
        .inject_headers(&mut aux, DispatchHeaderState::AUXILIARY)
        .unwrap();
    let mut main = adapter.client.prepare(&request).await.expect("prepare");
    adapter
        .inject_headers(&mut main, DispatchHeaderState::default())
        .unwrap();
    assert!(
        !aux.provider_request
            .headers
            .contains_key("anthropic-dispatch-id"),
        "auxiliary (side-query) traffic must not carry the header"
    );
    assert!(main
        .provider_request
        .headers
        .contains_key("anthropic-dispatch-id"));
}

#[tokio::test(start_paused = true)]
async fn physical_dispatch_287_http_recovery_and_query_reset() {
    let _lock = cedar_lattice_write();
    for cedar in [false, true] {
        let _cedar = cedar.then(CedarLatticeOn::set);
        for dreamy in [false, true] {
            let _dreamy = DreamyFrostGuard::set(dreamy);
            for source in ["sdk", "hook_prompt"] {
                for (streaming, side_query) in [(false, false), (true, false), (false, true)] {
                    for declined in [false, true] {
                        let transport = Arc::new(DispatchRecoveryTransport {
                            requests: Mutex::new(Vec::new()),
                            fault: DispatchFault::Http(503, declined),
                            failures: 1,
                        });
                        let mut service = make_adapter_with_retries(transport.clone(), Some(0));
                        service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
                        let sink = Arc::new(::telemetry::InMemorySink::new());
                        let bus = Arc::new(::telemetry::AnalyticsBus::new());
                        bus.attach_sink(sink.clone()).await;
                        service.analytics = Some(bus);
                        let first = if dreamy {
                            Some("v2d")
                        } else if cedar && source == "sdk" {
                            Some("v2s")
                        } else {
                            None
                        };
                        let start = tokio::time::Instant::now();
                        dispatch_result(&service, source, streaming, side_query)
                            .await
                            .unwrap();
                        let elapsed = start.elapsed();
                        assert_eq!(
                            transport.headers(),
                            [first.map(str::to_owned), Some("v2p".into())],
                            "{cedar}/{dreamy}/{source}/{streaming}/{side_query}/{declined}"
                        );
                        assert_eq!(
                            service.last_retry_count(),
                            0,
                            "dispatch recovery is outside the ordinary retry budget"
                        );
                        if first.is_none() && !declined {
                            assert!((Duration::from_millis(500)..=Duration::from_millis(625))
                                .contains(&elapsed));
                        } else {
                            assert_eq!(elapsed, Duration::ZERO);
                        }
                        let events = sink.events().await;
                        let events: Vec<_> = events
                            .iter()
                            .filter(|event| event.name == "tengu_dispatch_header_fallback")
                            .collect();
                        assert_eq!(events.len(), 1);
                        let metadata = &events[0].metadata;
                        for (key, value) in [
                            ("dispatch", first.unwrap_or("none")),
                            ("resend_dispatch", "v2p"),
                            ("query_source", source),
                            ("request_id", "dispatch-request"),
                        ] {
                            assert_eq!(
                                serde_json::to_value(&metadata[key]).unwrap(),
                                serde_json::Value::from(value)
                            );
                        }
                        assert_eq!(
                            serde_json::to_value(&metadata["status"]).unwrap(),
                            serde_json::Value::from(503)
                        );
                        let reason = if first.is_some() {
                            "5xx"
                        } else if declined {
                            "headerless_decline"
                        } else {
                            "headerless_retryable_503"
                        };
                        assert_eq!(
                            serde_json::to_value(&metadata["reason"]).unwrap(),
                            serde_json::Value::from(reason)
                        );
                        dispatch_result(&service, source, streaming, side_query)
                            .await
                            .unwrap();
                        assert_eq!(
                            transport.headers()[2].as_deref(),
                            first,
                            "the next query must reset its recovery state"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn physical_dispatch_287_recovery_latch_is_once_per_query() {
    let _lock = cedar_lattice_write();
    let _dreamy = DreamyFrostGuard::set(true);
    for fault in [
        DispatchFault::Http(500, false),
        DispatchFault::Connection,
        DispatchFault::Timeout,
        DispatchFault::Body(false),
    ] {
        for streaming in [false, true] {
            let transport = Arc::new(DispatchRecoveryTransport {
                requests: Mutex::new(Vec::new()),
                fault,
                failures: 2,
            });
            let service = make_adapter_with_retries(transport.clone(), Some(0));
            assert!(dispatch_result(&service, "sdk", streaming, false)
                .await
                .is_err());
            assert_eq!(
                transport.headers(),
                [Some("v2d".into()), Some("v2p".into())]
            );
        }
    }
    let transport = Arc::new(DispatchRecoveryTransport {
        requests: Mutex::new(Vec::new()),
        fault: DispatchFault::Body(true),
        failures: 1,
    });
    let service = make_adapter_with_retries(transport.clone(), Some(0));
    assert!(dispatch_result(&service, "sdk", true, false).await.is_err());
    assert_eq!(
        transport.headers(),
        [Some("v2d".into())],
        "forwarded events suppress the body-phase resend"
    );
}

#[tokio::test(start_paused = true)]
async fn model_validation_dispatch_header_recovery_obeys_sealed_replay_policy() {
    let _lock = cedar_lattice_write();
    let _dreamy = DreamyFrostGuard::set(true);
    for fault in [DispatchFault::Http(503, false), DispatchFault::Connection] {
        for hosted in [true, false] {
            let transport = Arc::new(DispatchRecoveryTransport {
                requests: Mutex::new(Vec::new()),
                fault,
                failures: 1,
            });
            let service = make_adapter_with_retries(transport.clone(), Some(0));
            let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
            request.input.max_tokens = Some(1);
            if hosted {
                request.input.hosted_tools.push(
                    lingxi_llm_client::protocol::HostedTool::WebSearch(Default::default()),
                );
            }
            let result = service.execute_model_validation_request(request).await;
            if hosted {
                let error = result.unwrap_err();
                assert!(!error.allows_automatic_replay(), "{error:?}");
                assert_eq!(transport.headers(), [Some("v2d".into())]);
            } else {
                result.unwrap();
                assert_eq!(
                    transport.headers(),
                    [Some("v2d".into()), Some("v2p".into())]
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn dispatch_287_does_not_add_recovery_to_other_provider_routes() {
    let _lock = cedar_lattice_write();
    let _dreamy = DreamyFrostGuard::set(true);
    for (protocol, provider, base) in [
        (
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://gateway.example.com",
        ),
        (
            ProtocolFamily::AnthropicMessages,
            ProviderId::OpenAICompatible {
                name: "anthropic-compatible".into(),
            },
            "https://api.anthropic.com",
        ),
        (
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAICompatible {
                name: "custom".into(),
            },
            "https://custom.example.com",
        ),
        (
            ProtocolFamily::OpenAiResponses,
            ProviderId::OpenAICompatible {
                name: "responses".into(),
            },
            "https://custom.example.com",
        ),
    ] {
        for streaming in [false, true] {
            let transport = Arc::new(DispatchRecoveryTransport {
                requests: Mutex::new(Vec::new()),
                fault: DispatchFault::Http(503, false),
                failures: 2,
            });
            let mut service = make_adapter_for_protocol_with_transport(
                protocol,
                provider.clone(),
                base,
                "p",
                "claude-sonnet-4-20250514",
                transport.clone(),
            );
            service.settings_max_retries = Some(0);
            assert!(dispatch_result(&service, "sdk", streaming, false)
                .await
                .is_err());
            assert_eq!(
                transport.headers(),
                [None],
                "{protocol:?}/{provider:?}/{streaming}"
            );
        }
    }
}

#[tokio::test]
async fn physical_stream_and_owned_main_dispatch_follow_native_query_source() {
    let _lock = cedar_lattice_write();
    let _cedar = CedarLatticeOn::set();
    for dreamy in [false, true] {
        let _dreamy = DreamyFrostGuard::set(dreamy);
        for (source, ordinary_v2s) in [
            (None, true),
            (Some("repl_main_thread"), true),
            (Some("repl_main_thread:resume"), true),
            (Some("sdk"), true),
            (Some("agent:worker"), true),
            (Some("hook_agent"), true),
            (Some("hook_prompt"), false),
            (Some("session_memory"), false),
        ] {
            for streaming in [false, true] {
                let transport = Arc::new(DispatchTrafficTransport {
                    requests: Mutex::new(Vec::new()),
                    fail_first: false,
                });
                let service = make_adapter_with_retries(transport.clone(), Some(0));
                service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
                run_dispatch_traffic(&service, source, streaming).await;
                let requests = transport.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                let expected = if dreamy {
                    Some("v2d")
                } else if ordinary_v2s {
                    Some("v2s")
                } else {
                    None
                };
                assert_eq!(
                    requests[0]
                        .headers
                        .get("anthropic-dispatch-id")
                        .map(String::as_str),
                    expected,
                    "source={source:?}, streaming={streaming}, dreamy={dreamy}"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn auxiliary_stream_and_owned_main_error_use_normal_retry_budget() {
    let _lock = cedar_lattice_write();
    let _cedar = CedarLatticeOn::set();
    let _dreamy = DreamyFrostGuard::set(false);
    for streaming in [false, true] {
        let transport = Arc::new(DispatchTrafficTransport {
            requests: Mutex::new(Vec::new()),
            fail_first: true,
        });
        let service = make_adapter_with_retries(transport.clone(), Some(1));
        service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
        run_dispatch_traffic(&service, Some("hook_prompt"), streaming).await;
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|request| !request.headers.contains_key("anthropic-dispatch-id")));
        assert_eq!(
            service.last_retry_count(),
            1,
            "auxiliary errors must consume the normal retry budget, without a free header-strip replay"
        );
    }
}

#[tokio::test]
async fn dispatch_recovery_resends_v2p_only_for_this_query() {
    let _g = cedar_lattice_write();
    let _on = CedarLatticeOn::set();
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    );
    let mut request = LlmRequest::new("model").with_user_text("hi");
    request.input.max_tokens = Some(4096);
    let mut query = DispatchHeaderState::default();
    let mut prepared = adapter.client.prepare(&request).await.unwrap();
    let (attempt, _) = adapter.inject_headers(&mut prepared, query).unwrap();
    assert_eq!(attempt.value.as_deref(), Some("v2s"));
    assert!(ApiService::note_dispatch_header_failure(
        &mut query,
        &attempt,
        &LlmError::RateLimited {
            retry_after: None,
            scope: None
        },
        Some((429, false)),
    )
    .is_none());
    let fallback = ApiService::note_dispatch_header_failure(
        &mut query,
        &attempt,
        &LlmError::ProviderInternal,
        Some((500, false)),
    )
    .unwrap();
    assert_eq!(fallback.reason, "5xx");
    assert_eq!(fallback.status, Some(500));
    assert_eq!(fallback.previous.as_deref(), Some("v2s"));
    assert!(!fallback.wait);
    let mut retry = adapter.client.prepare(&request).await.unwrap();
    let (retry_attempt, _) = adapter.inject_headers(&mut retry, query).unwrap();
    assert_eq!(
        retry
            .provider_request
            .headers
            .get(DISPATCH_ID_HEADER)
            .map(String::as_str),
        Some("v2p")
    );
    assert!(ApiService::note_dispatch_header_failure(
        &mut query,
        &retry_attempt,
        &LlmError::ProviderInternal,
        Some((503, false)),
    )
    .is_none());
    let mut next = adapter.client.prepare(&request).await.unwrap();
    adapter
        .inject_headers(&mut next, DispatchHeaderState::default())
        .unwrap();
    assert_eq!(
        next.provider_request
            .headers
            .get(DISPATCH_ID_HEADER)
            .map(String::as_str),
        Some("v2s")
    );
}

/// The drive loop's one-frame lookahead must resend v2p, emit `tengu_dispatch_header_fallback{reason:"body_phase",
/// status:"none"}` and re-open — and the seeded frame must flow through the
/// unfold untouched on the retry that succeeds.
#[tokio::test]
async fn dispatch_body_phase_drop_resends_v2p_and_re_opens() {
    use ::telemetry::AnalyticsValue;
    use futures::StreamExt as _;

    let _g = cedar_lattice_write();
    let _on = CedarLatticeOn::set();
    let transport = Arc::new(BodyPhaseDropThenOk {
        attempts: Mutex::new(Vec::new()),
    });
    let (adapter, sink) = make_stream_adapter_with_bus(transport.clone()).await;

    let mut stream = adapter
        .stream(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await
        .expect("stream open ok");
    let mut yielded = 0_usize;
    while let Some(item) = stream.next().await {
        assert!(item.is_ok(), "retry stream must decode cleanly: {item:?}");
        yielded += 1;
    }
    assert!(yielded > 0, "the seeded retry stream must yield events");

    let seen = transport
        .attempts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![Some("v2s".to_string()), Some("v2p".to_string())],
        "the body-phase retry must resend v2p"
    );

    let events = sink.events().await;
    let fallback = events
        .iter()
        .find(|e| e.name == "tengu_dispatch_header_fallback")
        .expect("body-phase fallback event must be emitted");
    assert!(
        matches!(&fallback.metadata["reason"], AnalyticsValue::String(s) if s == "body_phase"),
        "reason must be body_phase; got {:?}",
        fallback.metadata["reason"]
    );
    assert!(
        matches!(&fallback.metadata["status"], AnalyticsValue::String(s) if s == "none"),
        "status must stringify to none; got {:?}",
        fallback.metadata["status"]
    );
}

#[tokio::test]
async fn anthropic_beta_header_is_anthropic_family_and_provider_scoped() {
    let anthropic_headers = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
    .await;
    assert!(anthropic_headers.contains_key("anthropic-beta"));

    let gateway_headers = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://gateway.example",
    )
    .await;
    assert!(
        gateway_headers.contains_key("anthropic-beta"),
        "a first-party provider keeps its beta policy behind a gateway"
    );
    let compatible_headers = headers_after_inject_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::Custom {
            name: "compatible".into(),
        },
        "https://gateway.example",
    )
    .await;
    assert!(
        !compatible_headers.contains_key("anthropic-beta"),
        "an Anthropic-wire custom endpoint must not inherit first-party betas"
    );

    for (protocol, base_url, name) in [
        (
            ProtocolFamily::FoundryClaude,
            "https://example.services.ai.azure.com/anthropic",
            "foundry-claude",
        ),
        (
            ProtocolFamily::VertexClaude,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
            "vertex-claude",
        ),
        (
            ProtocolFamily::BedrockClaude,
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            "bedrock-claude",
        ),
    ] {
        let headers = headers_after_inject_for_protocol(
            protocol,
            ProviderId::OpenAICompatible {
                name: name.to_string(),
            },
            base_url,
        )
        .await;
        assert!(
            headers.contains_key("anthropic-beta"),
            "{name} must receive its provider-specific beta header: {headers:?}"
        );
    }

    let routes = [
        (
            ProtocolFamily::AnthropicMessages,
            "https://gateway.example",
            "anthropic-compatible",
        ),
        (
            ProtocolFamily::OpenAiChat,
            "https://api.openai.com/v1",
            "openai",
        ),
        (
            ProtocolFamily::OpenAiResponses,
            "https://api.openai.com/v1",
            "openai-responses",
        ),
        (
            ProtocolFamily::GeminiGenerateContent,
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini",
        ),
        (
            ProtocolFamily::AzureOpenAi,
            "https://example.openai.azure.com/openai/deployments/model",
            "azure-openai",
        ),
        (
            ProtocolFamily::VertexGemini,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/model:generateContent",
            "vertex-gemini",
        ),
    ];

    for (protocol, base_url, name) in routes {
        let headers = headers_after_inject_for_protocol(
            protocol,
            ProviderId::OpenAICompatible {
                name: name.to_string(),
            },
            base_url,
        )
        .await;
        assert!(
            !headers.contains_key("anthropic-beta"),
            "{name} must not receive anthropic-beta: {headers:?}"
        );
        assert!(headers.contains_key("user-agent"));
        assert!(
            !headers.contains_key("x-request-id"),
            "Host must not synthesize a provider request-id for {name}"
        );
        assert!(
            !headers.contains_key("x-client-request-id"),
            "the SDK's selected-route request-id policy is outside this Host-only injector"
        );
    }
}

#[tokio::test]
async fn user_agent_is_provider_aware() {
    // Anthropic-family routes keep the byte-faithful claude-cli UA.
    for (proto, pid, url) in [
        (
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        ),
        (
            ProtocolFamily::BedrockClaude,
            ProviderId::BedrockClaude,
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        ),
    ] {
        let h = headers_after_inject_for_protocol(proto, pid, url).await;
        let ua = h.get("user-agent").expect("ua");
        assert!(ua.starts_with("claude-cli/"), "anthropic-family UA: {ua}");
    }

    // Non-Anthropic routes get a neutral UA, never claude-cli.
    let h = headers_after_inject_for_protocol(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "openai".to_string(),
        },
        "https://api.openai.com/v1",
    )
    .await;
    let ua = h.get("user-agent").expect("ua");
    assert!(ua.starts_with("LingXi-Code/"), "neutral UA expected: {ua}");
    assert!(!ua.contains("claude-cli"), "must not leak claude-cli: {ua}");
}

#[tokio::test]
async fn anthropic_compatibility_version_keeps_neutral_product_identity() {
    for (protocol, provider, url, expected) in [
        (
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
            "claude-cli/2.1.293 (external, sdk-cli)",
        ),
        (
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAICompatible {
                name: "openai".into(),
            },
            "https://api.openai.com/v1",
            "LingXi-Code/0.12.0",
        ),
    ] {
        let mut adapter = make_adapter_for_protocol(protocol, provider, url)
            .with_anthropic_compatible_version("2.1.293")
            .with_anthropic_client_metadata(lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::claude_code_2_1_293("darwin", "aarch64"));
        if protocol == ProtocolFamily::AnthropicMessages {
            adapter = adapter.with_native_anthropic_endpoint(url);
        }
        adapter.ua = UserAgentEnv {
            user_type: Some("external".into()),
            entrypoint: Some("sdk-cli".into()),
            ..Default::default()
        };
        adapter.version = "0.12.0".into();
        let mut request = LlmRequest::new("model").with_user_text("hi");
        if protocol == ProtocolFamily::AnthropicMessages {
            request.input.max_tokens = Some(4096);
        }
        let mut prepared = adapter.client.prepare(&request).await.unwrap();
        adapter.apply_user_agent(&mut prepared);
        assert_eq!(prepared.provider_request.headers["user-agent"], expected);
        if protocol == ProtocolFamily::AnthropicMessages {
            assert_eq!(
                prepared.provider_request.headers["x-stainless-package-version"],
                "0.128.0"
            );
            assert_eq!(
                prepared.provider_request.headers["x-stainless-runtime-version"],
                "v26.3.0"
            );
            assert_eq!(prepared.provider_request.headers["x-stainless-os"], "MacOS");
            assert_eq!(
                prepared.provider_request.headers["x-stainless-arch"],
                "arm64"
            );
        } else {
            assert!(!prepared
                .provider_request
                .headers
                .keys()
                .any(|name| name.to_ascii_lowercase().starts_with("x-stainless-")));
        }
    }
}

#[test]
fn side_fork_keeps_native_thinking_overlay_without_generic_projection() {
    use crate::model::{thinking::ThinkingConfig, thinking_state::NativeIntegerNumber};

    let parent = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
    .with_thinking(ThinkingConfig::Adaptive);
    // Native Main accepts wider JS integer overlays than the generic API.
    parent.set_max_thinking_tokens_override(NativeIntegerNumber::new(-1.0));
    assert!(parent.thinking().is_err());
    let child = parent.fork_independent();
    assert!(child.thinking().is_err());
    child.set_max_thinking_tokens_override(None);
    assert_eq!(child.thinking().unwrap(), ThinkingConfig::Adaptive);
    assert!(
        parent.thinking().is_err(),
        "child overlay changes must stay private"
    );
}

#[tokio::test]
async fn side_fork_preserves_explicit_native_endpoint_admission() {
    use lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata;

    for selected in [false, true] {
        let mut parent = make_adapter_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .with_anthropic_client_metadata(AnthropicClientMetadata::claude_code_2_1_293(
            "darwin", "arm64",
        ));
        if selected {
            parent = parent.with_native_anthropic_endpoint("https://api.anthropic.com");
        }
        let child = parent.fork_independent();
        let mut request = LlmRequest::new("model").with_user_text("hello");
        request.input.max_tokens = Some(4096);
        let mut prepared = child.client.prepare(&request).await.unwrap();
        assert_eq!(
            child.native_anthropic_endpoint_selected(&prepared),
            selected
        );
        child.apply_user_agent(&mut prepared);
        assert_eq!(
            prepared
                .provider_request
                .headers
                .contains_key("x-stainless-package-version"),
            selected,
        );
        assert_eq!(
            prepared
                .wire_draft
                .as_ref()
                .unwrap()
                .request()
                .http1_header_layout,
            selected.then_some(lingxi_llm_client::Http1HeaderLayout::NativeFetch),
        );
    }
}

#[test]
fn explicit_api_identity_metadata_does_not_read_process_metadata() {
    let extra = serde_json::json!({"scope":"embedding","device_id":"stale"})
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        ApiService::build_api_metadata_user_id_with_extra(
            "device",
            "account",
            "session",
            Some("parent"),
            Some(extra)
        ),
        r#"{"scope":"embedding","device_id":"device","account_uuid":"account","session_id":"session","parent_session_id":"parent"}"#
    );
    assert_eq!(
        ApiService::build_api_metadata_user_id_with_extra(
            "device", "account", "session", None, None
        ),
        r#"{"device_id":"device","account_uuid":"account","session_id":"session"}"#
    );
}

#[tokio::test]
async fn request_session_snapshot_survives_clear_and_new_admissions_refresh() {
    let original = lingxi_core::types::SessionId::new().as_uuid().to_string();
    let replacement = lingxi_core::types::SessionId::new().as_uuid().to_string();
    let live = Arc::new(std::sync::Mutex::new(original.clone()));
    let metadata =
        ApiService::build_api_metadata_user_id_with_extra("device", "account", "boot", None, None);
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
    .with_request_metadata(crate::RequestMetadata { user_id: metadata.clone() })
    .with_anthropic_client_metadata(
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::claude_code_2_1_293("darwin", "aarch64"),
    )
    .with_native_anthropic_endpoint("https://api.anthropic.com");
    let owner = live.clone();
    adapter.set_request_session_id_source(Arc::new(move || {
        let owner = owner.clone();
        Box::pin(async move { Some(owner.lock().unwrap().clone()) })
    }));
    let mut admitted = LlmRequest::new("model").with_user_text("hi");
    admitted.input.max_tokens = Some(4096);
    admitted.input.metadata = lingxi_llm_client::protocol::JsonPayload::from_generated_value(
        &serde_json::json!({"user_id":metadata}),
    )
    .unwrap();
    adapter.capture_request_session_id(&mut admitted).await;
    *live.lock().unwrap() = replacement.clone();
    adapter.capture_request_session_id(&mut admitted).await;
    assert_eq!(
        admitted.execution.request_session_id.as_deref(),
        Some(original.as_str())
    );
    let mut prepared = adapter.client.prepare(&admitted).await.unwrap();
    adapter.apply_user_agent(&mut prepared);
    assert_eq!(
        prepared.provider_request.headers["x-claude-code-session-id"],
        original
    );
    let user_id = admitted
        .input
        .metadata
        .string_at("/user_id")
        .expect("request metadata retains the host user_id");
    let identity: serde_json::Value = serde_json::from_str(&user_id).unwrap();
    assert_eq!(identity["session_id"], original);
    let mut next = LlmRequest::new("model").with_user_text("next");
    adapter.capture_request_session_id(&mut next).await;
    assert_eq!(
        next.execution.request_session_id.as_deref(),
        Some(replacement.as_str())
    );
}

#[tokio::test]
async fn request_parent_scope_takes_precedence_over_current_session_and_payload() {
    let parent = lingxi_core::types::SessionId::new();
    let current = lingxi_core::types::SessionId::new().as_uuid().to_string();
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    );
    adapter.set_request_session_id_source(Arc::new(move || {
        let current = current.clone();
        Box::pin(async move { Some(current) })
    }));
    let mut request = LlmRequest::new("model").with_user_text("child");
    request.input.metadata =
        lingxi_llm_client::protocol::JsonPayload::parse(r#"{"session_id":"provider-authored-id"}"#)
            .unwrap();
    lingxi_core::host::session_flags::scope_request_session_id(parent, async {
        adapter.capture_request_session_id(&mut request).await;
    })
    .await;
    assert_eq!(
        request.execution.request_session_id,
        Some(parent.as_uuid().to_string())
    );
}

#[tokio::test]
async fn registered_request_origin_precedes_live_and_unrelated_task_scope() {
    let origin = lingxi_core::types::SessionId::new();
    let current = lingxi_core::types::SessionId::new();
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    );
    adapter.set_model_attempt_hooks(Arc::new(RegisteredRequestSession(origin)));
    adapter.set_request_session_id_source(Arc::new(|| {
        panic!("registered requests never consult current owner")
    }));
    let run = lingxi_core::host::ModelAttemptRun::new(Arc::new(()));
    let mut request = LlmRequest::new("model");
    request.execution.model_attempt = Some(
        run.context(lingxi_core::host::ModelAttemptStage::Panel, Some(0))
            .unwrap(),
    );
    lingxi_core::host::session_flags::scope_request_session_id(current, async {
        adapter.capture_request_session_id(&mut request).await;
    })
    .await;
    assert_eq!(
        request.execution.request_session_id,
        Some(origin.as_uuid().to_string())
    );
}

#[tokio::test]
async fn authenticator_user_agent_is_not_duplicated() {
    // Simulate the Copilot authenticator having set `User-Agent` during
    // prepare(): inject_headers must NOT add a second lowercase `user-agent`.
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "github-copilot".to_string(),
        },
        "https://api.githubcopilot.com",
    );
    let request = LlmRequest::new("model").with_user_text("hi");
    let mut prepared = adapter.client.prepare(&request).await.expect("prepare");
    prepared
        .provider_request
        .headers
        .insert("User-Agent".to_string(), "LingXi-Code".to_string());
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    let h = &prepared.provider_request.headers;
    assert_eq!(h.get("User-Agent").map(String::as_str), Some("LingXi-Code"));
    assert!(
        !h.contains_key("user-agent"),
        "no duplicate lowercase user-agent: {h:?}"
    );
}

/// Plan test: budget terminates after DEFAULT_MAX_RETRIES + 1 executions.
#[tokio::test]
async fn retry_terminal_after_budget() {
    // The drive loop's attempt budget is only stable while the dispatch
    // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
    // budget-free "strip and retry" attempt on the first 5xx.
    let _dispatch_off = cedar_lattice_read();
    // All responses are 500 — should retry DEFAULT_MAX_RETRIES times then fail.
    let transport = FakeTransport::sequence(vec![FakeResponse::Ok(ProviderResponse::json(
        500,
        serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "internal"}}),
    ))]);
    let adapter = make_adapter_with_routing(
        transport.clone(),
        std::collections::BTreeMap::new(),
        None,
        Some(0),
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
    assert!(result.is_err(), "must fail after exhausting budget");
    // Should have tried DEFAULT_MAX_RETRIES + 1 = 11 times.
    assert_eq!(
        u32::try_from(transport.seen_count()).unwrap(),
        DEFAULT_MAX_RETRIES + 1,
        "expected {} executions, got {}",
        DEFAULT_MAX_RETRIES + 1,
        transport.seen_count()
    );
}

/// Native headerless 503 decline gets one v2p retry, then is terminal.
#[tokio::test]
async fn headerless_503_decline_retries_v2p_once_then_is_terminal() {
    let _flags = cedar_lattice_read();
    let mut headers = BTreeMap::new();
    headers.insert("x-should-retry".to_string(), "false".to_string());

    let transport = FakeTransport::always(ProviderResponse {
        status: 503,
        headers,
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "api_error", "message": "service unavailable"}
        })),
        request_id: None,
    });
    let adapter = make_adapter(transport.clone());
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(result.is_err(), "x-should-retry:false must be terminal");
    assert_eq!(
        transport.seen_count(),
        2,
        "one native v2p resend, then terminal"
    );
    assert!(!transport.seen_headers(0).contains_key(DISPATCH_ID_HEADER));
    assert_eq!(
        transport
            .seen_headers(1)
            .get(DISPATCH_ID_HEADER)
            .map(String::as_str),
        Some("v2p")
    );
}

/// Plan test: tool_use id round-trips through the adapter without mangling.
#[tokio::test]
async fn tool_use_id_round_trip_within_turn() {
    let response_json = serde_json::json!({
        "id": "msg_tool",
        "model": "claude-sonnet-4-20250514",
        "content": [
            {"type": "tool_use", "id": "toolu_abc", "name": "Read", "input": {"path": "/x"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
    let adapter = make_adapter(transport);
    let resp = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("ok");
    match resp.content.as_slice() {
        [crate::ContentBlock::ToolCall { id, name, .. }] => {
            assert_eq!(id, "toolu_abc", "tool_use id must round-trip verbatim");
            assert_eq!(name, "Read");
        }
        other => panic!("expected single ToolCall, got: {other:?}"),
    }
}
