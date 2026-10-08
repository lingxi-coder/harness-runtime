//! Main-query integration checks for admitted server-fallback events.

use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig};
use lingxi_core::host::{refusal_server::Policy, OrchestratorHandle, ResumeRuntimeSnapshot};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, SessionId};
use llm_runtime::history::HistoryServerFallback;
use llm_runtime::model::allowlist::ModelEnforcement;
use llm_runtime::{ContentBlock as LlmContentBlock, HistoryResponse};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn fallback_info(
    from_model: &str,
    received_model: &str,
    declared_model: &str,
    reason: &str,
    category: Option<&str>,
    profile: &str,
    request_id: Option<&str>,
) -> HistoryServerFallback {
    serde_json::from_value(json!({
        "event": {
            "fromModel": from_model,
            "toModel": received_model,
            "reason": reason,
            "apiRefusalCategory": category,
            "midStream": false,
            "requestId": request_id,
            "discardedBlocks": [],
            "retainedBlocks": [],
            "retainedText": "",
            "finalStopReason": "end_turn"
        },
        "profile": profile,
        "lane": {
            "forModel": from_model,
            "model": declared_model,
            "mode": "explicit"
        }
    }))
    .expect("valid SDK server fallback observation")
}

fn attach_fallback(response: &mut HistoryResponse, info: &HistoryServerFallback) {
    response.provider_metadata = json!({
        "llm_client": {
            "server_fallback_events": [serde_json::to_value(info).unwrap()]
        }
    });
}

fn orchestrator(
    config: OrchestratorConfig,
    responses: Vec<HistoryResponse>,
) -> (
    ConversationOrchestrator,
    Arc<MockApiClient>,
    MockOutputStream,
) {
    let api = Arc::new(MockApiClient::new(responses));
    let output = MockOutputStream::new();
    let orch = ConversationOrchestrator::new(
        config,
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(output.clone()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    );
    (orch, api, output)
}

fn server_lane_policy(candidate: &str) -> Policy {
    Policy {
        candidate_model: Some(candidate.into()),
        enabled: true,
        switch_models_on_flag: true,
        server_allowed: true,
        explicit_target_eligible: true,
        beta_transport_enabled: true,
        ..Default::default()
    }
}

fn message_contains_text(message: &ConversationMessage, expected: &str) -> bool {
    match message {
        ConversationMessage::Assistant { content, .. }
        | ConversationMessage::User { content, .. } => content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains(expected))),
        ConversationMessage::System { content, .. } => content.contains(expected),
    }
}

#[test]
fn native_query_scope_uses_source_and_fork_origin() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/server_fallback_scope_2_1_288.json"
    ))
    .unwrap();
    for case in fixture["scopeCases"].as_array().unwrap() {
        let (main, local) = crate::server_fallback::query_scope(
            case["querySource"].as_str().unwrap(),
            case["forkOrigin"].as_str(),
        );
        assert_eq!(main, case["expected"]["isMainThread"].as_bool().unwrap());
        assert_eq!(
            local,
            case["expected"]["emitsLocalScope"].as_bool().unwrap()
        );
    }
}

#[tokio::test]
async fn native_acceptance_keeps_query_identity_and_snapshots_app_selection() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/server_fallback_scope_2_1_288.json"
    ))
    .unwrap();
    for case in fixture["acceptedCases"].as_array().unwrap() {
        let input = &case["input"];
        let expected = &case["expected"];
        let query_model = input["queryModel"].as_str().unwrap();
        let info = fallback_info(
            query_model,
            input["toModel"].as_str().unwrap(),
            input["declaredModel"].as_str().unwrap(),
            input["reason"].as_str().unwrap(),
            input["category"].as_str(),
            "serving-profile",
            Some("request"),
        );
        let (orch, _, _) = orchestrator(
            OrchestratorConfig {
                model: input["appModel"].as_str().unwrap().into(),
                query_source: if input["swapSession"].as_bool().unwrap() {
                    "sdk".into()
                } else {
                    "side_question".into()
                },
                ..Default::default()
            },
            Vec::new(),
        );
        crate::server_fallback::scope_query(async {
            crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                model: query_model.into(),
                profile: None,
            });
            assert_eq!(
                crate::server_fallback::handle(&orch, &info, false)
                    .await
                    .unwrap(),
                crate::server_fallback::ServerFallbackAdmission::Applied
            );
            assert_eq!(crate::server_fallback::query_state().fallback_count, 1);
            let notice = crate::server_fallback::flush_pending_notice(&orch).await;
            assert_eq!(notice.is_none(), expected["sameModel"].as_bool().unwrap());
        })
        .await;
        let session = orch.session.lock().await;
        assert_eq!(session.model, expected["appModel"].as_str().unwrap());
        for message in &session.history {
            if let ConversationMessage::System {
                refusal_fallback: Some(metadata),
                ..
            } = message
            {
                assert_eq!(
                    metadata.original_model,
                    expected["fromModel"].as_str().unwrap()
                );
            }
        }
        drop(session);
        let selection = orch.model_runtime.refusal_selection.lock().unwrap();
        assert_eq!(selection.header_armed, expected["armed"].as_bool().unwrap());
        if input["swapSession"].as_bool().unwrap() {
            assert_eq!(
                selection.live_latch().unwrap().previous_app_state_model,
                Some(Some(expected["previousAppModel"].as_str().unwrap().into()))
            );
        } else {
            assert!(selection.live_latch().is_none());
        }
    }
}

#[tokio::test]
async fn ordinary_child_has_no_notice_and_btw_child_has_a_local_notice() {
    for (fork_origin, expected_notice) in [(None, false), (Some("btw"), true)] {
        let (orch, _, _) = orchestrator(
            OrchestratorConfig {
                model: "original".into(),
                query_source: "agent:custom:reviewer".into(),
                fork_origin: fork_origin.map(str::to_owned),
                ..Default::default()
            },
            Vec::new(),
        );
        let info = fallback_info(
            "original", "served", "served", "sticky", None, "profile", None,
        );
        crate::server_fallback::scope_query(async {
            crate::server_fallback::handle(&orch, &info, false)
                .await
                .unwrap();
            assert_eq!(
                crate::server_fallback::flush_pending_notice(&orch)
                    .await
                    .is_some(),
                expected_notice
            );
        })
        .await;
        assert_eq!(orch.session.lock().await.model, "original");
    }
}

#[tokio::test]
async fn batched_cost_handoff_keeps_complete_and_incomplete_native_quotes_distinct() {
    for (quoted_usd, expected_nano, unpriced) in [
        (Some(0.012), 12_000_000, false),
        (Some(0.0), 0, false),
        (None, 0, true),
    ] {
        let mut response = mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        );
        response.usage = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
            input_tokens: 100,
            output_tokens: 20,
            server_tool_usage: Some(llm_runtime::ServerToolUsage {
                web_search_requests: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        });
        response.provider_metadata = json!({"llm_client": {"server_fallback_cost_quote": {
            "kind": "anthropic_server_fallback_per_iteration",
            "summaryModel": "served-model",
            "completeness": if quoted_usd.is_some() { "complete" } else { "incomplete" }
        }}});
        if let Some(amount) = quoted_usd {
            let mut estimate =
                llm_runtime::CostEstimate::unestimated(llm_runtime::PricingModelRef {
                    pricing_provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
                    billing_model: "served-model".into(),
                    request_model: "served-model".into(),
                    display_model: "served-model".into(),
                });
            estimate.estimated = true;
            estimate.total_cost_usd = Some(amount);
            response.cost = Some(estimate);
        }
        let (orch, _, _) = orchestrator(OrchestratorConfig::default(), vec![response]);
        let (persist_tx, _legacy_rx) = tokio::sync::mpsc::channel(8);
        let tracker = Arc::new(cost::CostTracker::new(
            SessionId::new(),
            Arc::new(cost::PricingCatalog::empty()),
            persist_tx,
        ));
        let orch = orch.with_cost_tracker(tracker.clone());
        orch.run_turn("quote handoff").await.unwrap();
        let state = tracker.snapshot().await;
        assert_eq!(state.total_nano_usd, expected_nano);
        assert_eq!(!state.unpriced_models.is_empty(), unpriced);
        assert!(state
            .per_model_usage
            .keys()
            .all(|model| model.model == "served-model"));
        assert_eq!(state.total_web_search_requests, 2);
    }
}

fn server_notice_message(
    original_model: &str,
    fallback_model: &str,
    previous_profile: Option<&str>,
    serving_profile: Option<&str>,
) -> ConversationMessage {
    ConversationMessage::System {
        id: MessageId::new(),
        content: "native server fallback notice".into(),
        subtype: Some("model_refusal_fallback".into()),
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: Some(lingxi_core::types::RefusalFallbackMetadata {
            trigger: "refusal".into(),
            direction: "retry".into(),
            scope: Some("session".into()),
            original_model: original_model.into(),
            fallback_model: fallback_model.into(),
            api_refusal_category: Some("cyber".into()),
            api_refusal_explanation: None,
            previous_profile: Some(previous_profile.map(str::to_owned)),
            serving_profile: Some(serving_profile.map(str::to_owned)),
            ..Default::default()
        }),
    }
}

#[test]
fn s_h_o_notice_copy_matches_native_288_fixture_cases() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/server_fallback_consumer_2_1_288.json"
    ))
    .expect("native server-fallback consumer fixture");
    for case in fixture["consumerCases"].as_array().unwrap() {
        let Some(expected) = case["notice"]["content"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let scope = match case["control"]["bannerScope"].as_str().unwrap() {
            "session" => lingxi_core::host::refusal_server_control::BannerScope::Session,
            "local" => lingxi_core::host::refusal_server_control::BannerScope::Local,
            other => panic!("unexpected native banner scope: {other}"),
        };
        let retained_text = if case["input"]["retainedTextState"].as_str() == Some("nonempty") {
            "Kept prefix"
        } else {
            ""
        };
        let actual = crate::query_model::server_fallback_notice_text(
            crate::query_model::ServerFallbackNoticeFacts {
                from_label: "Origin",
                to_label: "Fallback",
                category: case["notice"]["apiRefusalCategory"].as_str(),
                scope,
                retained_text,
                feedback_available: false,
                learn_more_url: "https://support.claude.com/en/articles/8106465",
                first_time_prefix: None,
                switched_to_line: None,
            },
        );
        assert_eq!(actual, expected, "{}", case["input"]["name"]);
    }
}

#[test]
fn server_notice_renderer_accepts_native_feedback_and_model_trait_facts() {
    use lingxi_core::host::refusal_server_control::BannerScope;

    let actual = crate::query_model::server_fallback_notice_text(
        crate::query_model::ServerFallbackNoticeFacts {
            from_label: "Origin",
            to_label: "Fallback",
            category: Some("cyber"),
            scope: BannerScope::Session,
            retained_text: "",
            feedback_available: true,
            learn_more_url: "https://support.claude.com/en/articles/16049681",
            first_time_prefix: Some("Custom first-time safeguards copy."),
            switched_to_line: Some(
                "Fallback is answering instead, or you can edit and retry with Origin.",
            ),
        },
    );

    assert_eq!(
        actual,
        "Custom first-time safeguards copy. Fallback is answering instead, or you can edit and retry with Origin. Send feedback with /feedback or learn more: https://support.claude.com/en/articles/16049681\n\nDetails: `[cyber]`"
    );
}

#[test]
fn server_notice_category_details_use_native_sanitization_and_utf16_cap() {
    use lingxi_core::host::refusal_server_control::BannerScope;

    let raw = "\u{001b}[31mcyber[role]`x`\u{200B}\u{2028}\u{2800}y\u{001b}[0m";
    let text = crate::query_model::server_fallback_notice_text(
        crate::query_model::ServerFallbackNoticeFacts {
            from_label: "Origin",
            to_label: "Fallback",
            category: Some(raw),
            scope: BannerScope::Session,
            retained_text: "",
            feedback_available: false,
            learn_more_url: "https://example.test/help",
            first_time_prefix: None,
            switched_to_line: None,
        },
    );
    assert!(text.ends_with("Details: `[cyberrolex y]`"));

    let empty = crate::query_model::server_fallback_notice_text(
        crate::query_model::ServerFallbackNoticeFacts {
            from_label: "Origin",
            to_label: "Fallback",
            category: Some("[]`"),
            scope: BannerScope::Session,
            retained_text: "",
            feedback_available: false,
            learn_more_url: "https://example.test/help",
            first_time_prefix: None,
            switched_to_line: None,
        },
    );
    assert!(!empty.contains("Details:"));

    // Native `re(text, 255)` first slices UTF-16, then backs off when the last
    // retained unit is a high surrogate. The 254 ASCII units plus this astral
    // pair therefore produce exactly the 254-character prefix; Rust reaches
    // the same representable result by refusing to split the `char`.
    let long = "a".repeat(254) + "😀z";
    let truncated = crate::query_model::server_fallback_notice_text(
        crate::query_model::ServerFallbackNoticeFacts {
            from_label: "Origin",
            to_label: "Fallback",
            category: Some(&long),
            scope: BannerScope::Session,
            retained_text: "",
            feedback_available: false,
            learn_more_url: "https://example.test/help",
            first_time_prefix: None,
            switched_to_line: None,
        },
    );
    assert!(truncated.ends_with(&format!("Details: `[{}]`", "a".repeat(254))));
}

#[test]
fn persisted_profile_extensions_keep_absent_distinct_from_explicit_null() {
    let present_null: lingxi_core::types::RefusalFallbackMetadata = serde_json::from_value(json!({
        "trigger":"refusal",
        "direction":"retry",
        "originalModel":"origin",
        "fallbackModel":"fallback",
        "previousProfile":null,
        "servingProfile":"first-party"
    }))
    .unwrap();
    assert_eq!(present_null.previous_profile, Some(None));
    assert_eq!(
        present_null.serving_profile,
        Some(Some("first-party".into()))
    );
    let encoded = serde_json::to_value(&present_null).unwrap();
    assert!(encoded.get("previousProfile").unwrap().is_null());

    let absent: lingxi_core::types::RefusalFallbackMetadata = serde_json::from_value(json!({
        "trigger":"refusal",
        "direction":"retry",
        "originalModel":"origin",
        "fallbackModel":"fallback"
    }))
    .unwrap();
    assert_eq!(absent.previous_profile, None);
    assert!(!serde_json::to_value(absent)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("previousProfile"));
}

#[tokio::test]
async fn hot_resume_restores_native_session_latch_and_host_profile_snapshot() {
    let source = "claude-opus-4-8";
    let fallback = "claude-sonnet-4-6";
    let source_profile = "origin-provider";
    let fallback_profile = "first-party-anthropic";
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            ..Default::default()
        },
        Vec::new(),
    );
    let orch = Arc::new(orch);
    orch.attach_owned_session_switches();
    let notice = server_notice_message(
        source,
        fallback,
        Some(source_profile),
        Some(fallback_profile),
    );
    let session_id = SessionId::from_uuid(uuid::Uuid::new_v4());
    OrchestratorHandle::resume_session(
        orch.as_ref(),
        session_id,
        vec![notice],
        None,
        None,
        ResumeRuntimeSnapshot {
            model: fallback.into(),
            model_profile: Some(fallback_profile.into()),
            ..Default::default()
        },
    )
    .await
    .expect("hot resume");

    {
        let session = orch.session.lock().await;
        assert_eq!(session.model, fallback);
        assert_eq!(session.model_profile.as_deref(), Some(fallback_profile));
    }
    {
        let selection = orch.model_runtime.refusal_selection.lock().unwrap();
        let latch = selection.live_latch().expect("restored refusal latch");
        assert_eq!(latch.previous_app_state_model, Some(Some(source.into())));
        assert_eq!(latch.previous_profile.as_deref(), Some(source_profile));
        assert!(selection.header_armed);
    }

    OrchestratorHandle::clear_session(orch.as_ref())
        .await
        .expect("clear after hot resume");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert_eq!(session.model_profile.as_deref(), Some(source_profile));
}

#[tokio::test]
async fn hot_resume_with_empty_snapshot_keeps_live_server_route_and_rehydrates_latch() {
    let source = "model-a";
    let fallback = "model-b";
    let source_profile = "origin-provider";
    let fallback_profile = "fallback-provider";
    let info = fallback_info(
        source,
        fallback,
        fallback,
        "refusal",
        Some("cyber"),
        fallback_profile,
        Some("hot-resume-request"),
    );
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_refusal_fallback: Some(server_lane_policy(fallback)),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec![fallback.into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );
    let orch = Arc::new(orch);
    orch.attach_owned_session_switches();
    orch.seed_initial_model_profile(source, source_profile)
        .await;
    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: Some(source_profile.into()),
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &info, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        crate::server_fallback::flush_pending_notice(&orch).await;
    })
    .await;
    let history = orch.session.lock().await.history.clone();

    OrchestratorHandle::resume_session(
        orch.as_ref(),
        SessionId::from_uuid(uuid::Uuid::new_v4()),
        history,
        None,
        None,
        ResumeRuntimeSnapshot::default(),
    )
    .await
    .expect("hot resume without a model snapshot");

    {
        let session = orch.session.lock().await;
        assert_eq!(session.model, fallback);
        assert_eq!(session.model_profile.as_deref(), Some(fallback_profile));
    }
    {
        let selection = orch.model_runtime.refusal_selection.lock().unwrap();
        let latch = selection.live_latch().expect("server route latch restored");
        assert_eq!(latch.previous_app_state_model, Some(Some(source.into())));
        assert_eq!(latch.previous_profile.as_deref(), Some(source_profile));
        assert!(selection.header_armed);
    }

    orch.clear_session()
        .await
        .expect("clear hot-resumed session");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert_eq!(session.model_profile.as_deref(), Some(source_profile));
}

#[tokio::test]
async fn cold_resume_uses_assistant_model_and_matching_notice_profiles_for_server_latch() {
    use platform_posix::fs::PosixFileSystem;
    use session::JsonlMessage;

    let source = "claude-opus-4-8";
    let fallback = "claude-sonnet-4-6";
    let source_profile = "origin-provider";
    let fallback_profile = "first-party-anthropic";
    let temp = tempfile::tempdir().unwrap();
    let session_uuid = uuid::Uuid::new_v4();
    let cwd = temp.path().join("project");
    let cwd_string = cwd.to_string_lossy().into_owned();
    let user_uuid = uuid::Uuid::new_v4().to_string();
    let notice_uuid = uuid::Uuid::new_v4().to_string();
    let assistant_uuid = uuid::Uuid::new_v4().to_string();
    let rows = [
        json!({
            "type":"user",
            "uuid":user_uuid,
            "parentUuid":null,
            "message":{"role":"user","content":[{"type":"text","text":"hello"}]}
        }),
        json!({
            "type":"system",
            "uuid":notice_uuid,
            "parentUuid":user_uuid,
            "subtype":"model_refusal_fallback",
            "direction":"retry",
            "scope":"session",
            "content":"native server fallback notice",
            "level":"warning",
            "trigger":"refusal",
            "originalModel":source,
            "fallbackModel":fallback,
            "apiRefusalCategory":"cyber",
            "apiRefusalExplanation":null,
            "previousProfile":source_profile,
            "servingProfile":fallback_profile,
            "isMeta":false
        }),
        json!({
            "type":"assistant",
            "uuid":assistant_uuid,
            "parentUuid":notice_uuid,
            "message":{
                "role":"assistant",
                "model":fallback,
                "content":[{"type":"text","text":"fallback answer"}]
            }
        }),
    ]
    .into_iter()
    .map(|mut row| {
        let object = row.as_object_mut().unwrap();
        object.insert("sessionId".into(), json!(session_uuid.to_string()));
        object.insert("timestamp".into(), json!("2026-10-03T12:34:56.000Z"));
        object.insert("cwd".into(), json!(cwd_string));
        object.insert("version".into(), json!("2.1.288"));
        object.insert("isSidechain".into(), json!(false));
        serde_json::from_value::<JsonlMessage>(row).unwrap()
    })
    .collect::<Vec<_>>();
    let transcript_path =
        session::jsonl::session_path(temp.path(), &cwd_string, &session_uuid.to_string());
    tokio::fs::create_dir_all(transcript_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(
        &transcript_path,
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect::<String>(),
    )
    .await
    .unwrap();

    let adapter = Arc::new(MockApiClient::new(Vec::new()));
    let orch = ConversationOrchestrator::with_resume(
        OrchestratorConfig {
            model: source.into(),
            ..Default::default()
        },
        session_uuid,
        temp.path().to_owned(),
        cwd_string,
        Arc::new(PosixFileSystem::new(temp.path().to_owned())),
        adapter,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
        None,
    )
    .await
    .expect("cold resume");
    let orch = Arc::new(orch);
    orch.attach_owned_session_switches();

    {
        let session = orch.session.lock().await;
        assert_eq!(session.model, fallback);
        assert_eq!(session.model_profile.as_deref(), Some(fallback_profile));
    }
    {
        let selection = orch.model_runtime.refusal_selection.lock().unwrap();
        let latch = selection.live_latch().expect("cold-restored latch");
        assert_eq!(latch.previous_app_state_model, Some(Some(source.into())));
        assert_eq!(latch.previous_model_for_session, Some(None));
        assert_eq!(latch.previous_profile.as_deref(), Some(source_profile));
        assert!(selection.header_armed);
    }

    OrchestratorHandle::clear_session(orch.as_ref())
        .await
        .expect("clear after cold resume");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert_eq!(session.model_profile.as_deref(), Some(source_profile));
}

#[tokio::test]
async fn cold_resume_notice_without_fallback_assistant_does_not_select_model_or_profile() {
    use platform_posix::fs::PosixFileSystem;
    use session::JsonlMessage;

    let launch_model = "claude-opus-4-8";
    let fallback_model = "claude-sonnet-4-6";
    let temp = tempfile::tempdir().unwrap();
    let session_uuid = uuid::Uuid::new_v4();
    let cwd = temp.path().join("project");
    let cwd_string = cwd.to_string_lossy().into_owned();
    let user_uuid = uuid::Uuid::new_v4().to_string();
    let assistant_uuid = uuid::Uuid::new_v4().to_string();
    let notice_uuid = uuid::Uuid::new_v4().to_string();
    let rows = [
        json!({
            "type":"user",
            "uuid":user_uuid,
            "parentUuid":null,
            "message":{"role":"user","content":[{"type":"text","text":"hello"}]}
        }),
        json!({
            "type":"assistant",
            "uuid":assistant_uuid,
            "parentUuid":user_uuid,
            "modelProfile":"origin-provider",
            "message":{
                "role":"assistant",
                "model":launch_model,
                "content":[{"type":"text","text":"origin answer"}]
            }
        }),
        json!({
            "type":"system",
            "uuid":notice_uuid,
            "parentUuid":assistant_uuid,
            "subtype":"model_refusal_fallback",
            "direction":"retry",
            "scope":"session",
            "content":"native server fallback notice",
            "level":"warning",
            "trigger":"refusal",
            "originalModel":launch_model,
            "fallbackModel":fallback_model,
            "apiRefusalCategory":"cyber",
            "apiRefusalExplanation":null,
            "previousProfile":"origin-provider",
            "servingProfile":"first-party-anthropic",
            "isMeta":false
        }),
    ]
    .into_iter()
    .map(|mut row| {
        let object = row.as_object_mut().unwrap();
        object.insert("sessionId".into(), json!(session_uuid.to_string()));
        object.insert("timestamp".into(), json!("2026-10-03T12:34:56.000Z"));
        object.insert("cwd".into(), json!(cwd_string));
        object.insert("version".into(), json!("2.1.288"));
        object.insert("isSidechain".into(), json!(false));
        serde_json::from_value::<JsonlMessage>(row).unwrap()
    })
    .collect::<Vec<_>>();
    let transcript_path =
        session::jsonl::session_path(temp.path(), &cwd_string, &session_uuid.to_string());
    tokio::fs::create_dir_all(transcript_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(
        &transcript_path,
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect::<String>(),
    )
    .await
    .unwrap();

    let orch = ConversationOrchestrator::with_resume(
        OrchestratorConfig {
            model: launch_model.into(),
            ..Default::default()
        },
        session_uuid,
        temp.path().to_owned(),
        cwd_string,
        Arc::new(PosixFileSystem::new(temp.path().to_owned())),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
        None,
    )
    .await
    .expect("cold resume");

    let session = orch.session.lock().await;
    assert_eq!(session.model, launch_model);
    assert_eq!(session.model_profile.as_deref(), Some("origin-provider"));
    drop(session);
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    assert!(selection.live_latch().is_none());
    assert!(selection.header_armed);
}

#[test]
fn cold_profile_extension_does_not_cross_launch_model_identity() {
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: "launch-model".into(),
            ..Default::default()
        },
        Vec::new(),
    );
    let history = vec![server_notice_message(
        "session-origin",
        "session-fallback",
        Some("origin-provider"),
        Some("fallback-provider"),
    )];

    assert!(crate::query_model::restore_refusal_selection(
        &orch,
        Some("session-fallback"),
        &history,
        false,
    ));
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    let latch = selection
        .live_latch()
        .expect("fallback matches selected route");
    assert_eq!(
        latch.previous_app_state_model,
        Some(Some("launch-model".into()))
    );
    assert!(latch.previous_profile.is_none());
}

#[test]
fn cold_profile_extension_does_not_reuse_earlier_origin_after_multi_hop() {
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: "model-a".into(),
            ..Default::default()
        },
        Vec::new(),
    );
    let history = vec![
        server_notice_message("model-a", "model-b", Some("profile-a"), Some("profile-b")),
        server_notice_message("model-b", "model-c", Some("profile-a"), Some("profile-c")),
    ];

    assert!(crate::query_model::restore_refusal_selection(
        &orch,
        Some("model-c"),
        &history,
        false,
    ));
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    let latch = selection.live_latch().expect("latest fallback matches");
    assert_eq!(latch.previous_app_state_model, Some(Some("model-a".into())));
    assert!(latch.previous_profile.is_none());
}

#[tokio::test]
async fn accepted_event_routes_next_request_and_clear_restores_model_and_profile() {
    let source = "claude-opus-4-8";
    let declared = "claude-sonnet-4-8[2m]";
    let received = "claude-sonnet-4-8";
    let profile = "first-party-anthropic";
    let info = fallback_info(
        source,
        received,
        declared,
        "refusal",
        Some("cyber"),
        profile,
        Some("server-request-1"),
    );
    let mut first = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "server response".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    attach_fallback(&mut first, &info);
    let second = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "next request".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let (orch, api, output) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            max_turns: 4,
            server_refusal_fallback: Some(server_lane_policy(declared)),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec!["sonnet".into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        vec![first, second],
    );
    orch.seed_initial_model_profile(source, "original-provider")
        .await;

    orch.run_turn("first turn").await.expect("first turn");
    let (notice_message, session_id) = {
        let session = orch.session.lock().await;
        assert_eq!(session.model, declared);
        assert_eq!(session.model_profile.as_deref(), Some(profile));
        let notice_message = session
            .history
            .iter()
            .find(|message| {
                matches!(message,
                    ConversationMessage::System {
                        subtype: Some(subtype),
                        refusal_fallback: Some(_),
                        ..
                    } if subtype == "model_refusal_fallback"
                )
            })
            .expect("accepted server hop persists a typed notice")
            .clone();
        let ConversationMessage::System {
            refusal_fallback: Some(notice),
            ..
        } = &notice_message
        else {
            unreachable!()
        };
        assert_eq!(notice.scope.as_deref(), Some("session"));
        assert_eq!(notice.original_model, source);
        assert_eq!(notice.fallback_model, declared);
        assert_eq!(notice.trigger, "refusal");
        assert_eq!(notice.direction, "retry");
        assert_eq!(notice.api_refusal_explanation, None);
        assert_eq!(
            notice.previous_profile.as_ref(),
            Some(&Some("original-provider".into()))
        );
        assert_eq!(notice.serving_profile.as_ref(), Some(&Some(profile.into())));
        (notice_message, session.session_id.to_string())
    };
    let row = orch.to_jsonl_message(&notice_message, &session_id, None, None, None, None);
    let encoded = serde_json::to_value(row).unwrap();
    assert_eq!(encoded["apiRefusalExplanation"], Value::Null);
    assert_eq!(encoded["previousProfile"], "original-provider");
    assert_eq!(encoded["servingProfile"], profile);
    let notices: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::SystemNotice { body, is_error } => {
                Some((body, is_error))
            }
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(!notices[0].1);
    assert!(notices[0].0.contains("Switched to"));
    {
        let selection = orch.model_runtime.refusal_selection.lock().unwrap();
        assert_eq!(selection.override_model, Some(Some(declared.into())));
        assert!(!selection.refusal_occurred);
        assert!(selection.header_armed);
        assert_eq!(
            selection.origin_request_id.as_deref(),
            Some("server-request-1")
        );
        let latch = selection
            .live_latch()
            .expect("server route is session-latched");
        assert_eq!(latch.previous_app_state_model, Some(Some(source.into())));
        assert_eq!(latch.previous_profile.as_deref(), Some("original-provider"));
    }
    assert!(!orch.model_runtime.refusal_cascade.lock().await.is_latched());

    orch.run_turn("second turn").await.expect("second turn");
    let requests = api.captured_requests().await;
    let main_requests: Vec<_> = requests
        .iter()
        .filter_map(|request| match request {
            crate::OrchestratorApiRequest::Main(request) => Some(request),
            crate::OrchestratorApiRequest::HookPrompt(_) => None,
        })
        .collect();
    assert_eq!(main_requests.len(), 2);
    assert_eq!(main_requests[1].model, declared);
    assert_eq!(main_requests[1].profile.as_deref(), Some(profile));

    orch.clear_session().await.expect("clear session");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert_eq!(session.model_profile.as_deref(), Some("original-provider"));
    drop(session);
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    assert!(selection.live_latch().is_none());
    assert!(!selection.header_armed);
}

#[tokio::test]
async fn same_physical_request_can_retarget_owned_latch_and_clear_restores_first_route() {
    let source = "claude-opus-4-8";
    let first_route = "claude-sonnet-4-8[2m]";
    let second_route = "claude-haiku-4-8[1m]";
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec!["sonnet".into(), "haiku".into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );
    orch.seed_initial_model_profile(source, "original-provider")
        .await;

    // Both observations belong to the same physical request: its lane keeps
    // `for_model = source` even after the first accepted route changes the
    // session to Sonnet. Keep the logical source profile in the query-local
    // request snapshot; each event's `profile` is the resolved route profile.
    let first = fallback_info(
        source,
        "claude-sonnet-4-8",
        first_route,
        "sticky",
        None,
        "sonnet-provider",
        Some("server-request-multi-hop"),
    );
    let second = fallback_info(
        source,
        "claude-haiku-4-8",
        second_route,
        "sticky",
        None,
        "haiku-provider",
        Some("server-request-multi-hop"),
    );
    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: Some("original-provider".into()),
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &first, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        assert_eq!(
            crate::server_fallback::handle(&orch, &second, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        crate::server_fallback::flush_pending_notice(&orch)
            .await
            .expect("latest query-local notice flushes once");
    })
    .await;

    {
        let session = orch.session.lock().await;
        assert_eq!(session.model, second_route);
        assert_eq!(session.model_profile.as_deref(), Some("haiku-provider"));
        let notices = session
            .history
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::System {
                    refusal_fallback: Some(metadata),
                    ..
                } => Some(metadata),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1, "Native Q keeps only the latest notice");
        assert_eq!(notices[0].fallback_model, second_route);
    }
    {
        let selection = orch.model_runtime.refusal_selection.lock().unwrap();
        let latch = selection
            .live_latch()
            .expect("second hop remains server-latched");
        assert_eq!(latch.previous_model_for_session, Some(Some(source.into())));
        assert_eq!(latch.previous_profile.as_deref(), Some("original-provider"));
    }

    orch.clear_session().await.expect("clear session");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert_eq!(session.model_profile.as_deref(), Some("original-provider"));
}

#[tokio::test]
async fn same_model_server_hop_keeps_session_effects_but_suppresses_the_notice() {
    let source = "claude-opus-4-8";
    let info = fallback_info(
        source,
        source,
        source,
        "sticky",
        Some("cyber"),
        "first-party-anthropic",
        Some("server-request-same-model"),
    );
    let (orch, _, output) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            ..Default::default()
        },
        Vec::new(),
    );
    orch.seed_initial_model_profile(source, "first-party-anthropic")
        .await;

    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: Some("first-party-anthropic".into()),
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &info, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
    })
    .await;

    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert_eq!(
        session.model_profile.as_deref(),
        Some("first-party-anthropic")
    );
    assert!(!session.history.iter().any(|message| matches!(
        message,
        ConversationMessage::System {
            subtype: Some(subtype),
            refusal_fallback: Some(_),
            ..
        } if subtype == "model_refusal_fallback"
    )));
    drop(session);
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    assert!(selection.live_latch().is_some());
    // Sticky hops still update the session route, but only refusal+cyber
    // observations arm the refusal request header (native 2.1.288 oracle).
    assert!(!selection.header_armed);
    drop(selection);
    assert!(!output
        .snapshot()
        .await
        .iter()
        .any(|event| matches!(event, lingxi_core::host::OutputEvent::SystemNotice { .. })));
}

#[tokio::test]
async fn local_server_notice_is_visible_and_persisted_without_mutating_session_route() {
    let dir = tempfile::tempdir().unwrap();
    let jsonl_path = dir.path().join("server-fallback.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(platform_posix::fs::PosixFileSystem::new(dir.path().into()));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        jsonl_path.clone(),
        fs,
    ));
    let source = "claude-opus-4-8";
    let target = "claude-sonnet-4-6";
    let mut info = fallback_info(
        source,
        target,
        target,
        "sticky",
        None,
        "first-party-anthropic",
        Some("server-request-local"),
    );
    info.event.mid_stream = true;
    info.event.retained_text = "kept prefix".into();
    let (orch, _, output) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            query_source: "side_question".into(),
            ..Default::default()
        },
        Vec::new(),
    );
    let orch = orch.with_jsonl_writer(writer);
    let route = crate::query_model::ModelRoute {
        model: source.into(),
        profile: Some("first-party-anthropic".into()),
    };
    crate::query_model::ROUTE
        .scope(
            Some(route.clone()),
            crate::server_fallback::scope_query(async {
                crate::server_fallback::record_request_route(route);
                assert_eq!(
                    crate::server_fallback::handle(&orch, &info, false)
                        .await
                        .unwrap(),
                    crate::server_fallback::ServerFallbackAdmission::Applied
                );
                assert!(crate::server_fallback::flush_pending_notice(&orch)
                    .await
                    .is_some());
            }),
        )
        .await;

    let (notice, notice_timestamp) = {
        let session = orch.session.lock().await;
        assert_eq!(session.model, source);
        assert!(session.model_profile.is_none());
        let notice = session
            .history
            .iter()
            .find(|message| {
                matches!(
                    message,
                    ConversationMessage::System {
                        subtype: Some(subtype),
                        refusal_fallback: Some(_),
                        ..
                    } if subtype == "model_refusal_fallback"
                )
            })
            .expect("local notice is a typed history row")
            .clone();
        let notice_timestamp = match &notice {
            ConversationMessage::System {
                refusal_fallback: Some(metadata),
                ..
            } => metadata
                .notice_timestamp
                .clone()
                .expect("queued notice captures enqueue time"),
            _ => unreachable!(),
        };
        (notice, notice_timestamp)
    };
    let message_id = notice.id().as_uuid().to_string();
    let persisted = std::fs::read_to_string(&jsonl_path).expect("flushed JSONL notice");
    let encoded: Value = persisted
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
        .find(|row: &Value| row["uuid"] == message_id)
        .expect("queued notice was persisted");
    assert_eq!(encoded["type"], "system");
    assert_eq!(encoded["subtype"], "model_refusal_fallback");
    assert_eq!(encoded["scope"], "local");
    assert_eq!(encoded["originalModel"], source);
    assert_eq!(encoded["fallbackModel"], target);
    assert_eq!(encoded["apiRefusalExplanation"], Value::Null);
    assert_eq!(encoded["timestamp"], notice_timestamp);
    assert!(!encoded.as_object().unwrap().contains_key("noticeTimestamp"));

    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    assert!(selection.live_latch().is_none());
    assert!(!selection.header_armed);
    drop(selection);
    let notices: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::SystemNotice { body, .. } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("This response was completed by"));
    assert!(notices[0].contains("Your session model is unchanged."));
}

#[tokio::test]
async fn query_scope_flushes_pending_notice_on_error_return() {
    let source = "model-a";
    let target = "model-b";
    let info = fallback_info(
        source,
        target,
        target,
        "sticky",
        None,
        "provider-b",
        Some("queued-notice-error-return"),
    );
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec![target.into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );
    let result = crate::server_fallback::scope_query_and_flush(&orch, async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: None,
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &info, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        Err::<(), _>("query ended with an error")
    })
    .await;
    assert_eq!(result, Err("query ended with an error"));

    let session = orch.session.lock().await;
    let notices = session
        .history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::System {
                refusal_fallback: Some(metadata),
                ..
            } => Some(metadata),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "queued notice survives error return");
    assert_eq!(notices[0].fallback_model, target);
}

#[tokio::test]
async fn server_multihops_in_later_query_preserve_first_restore_route() {
    let original = "model-a";
    let first_route = "model-b";
    let second_route = "model-d";
    let final_route = "model-e";
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: original.into(),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec![first_route.into(), second_route.into(), final_route.into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );
    orch.seed_initial_model_profile(original, "original-provider")
        .await;

    let first_query_hop = fallback_info(
        original,
        first_route,
        first_route,
        "sticky",
        None,
        "provider-b",
        Some("request-a-to-b"),
    );
    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: original.into(),
            profile: Some("original-provider".into()),
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &first_query_hop, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        crate::server_fallback::flush_pending_notice(&orch)
            .await
            .expect("first query notice flushes");
    })
    .await;

    let second_query_first_hop = fallback_info(
        first_route,
        second_route,
        second_route,
        "sticky",
        None,
        "provider-d",
        Some("request-b-to-d-to-e"),
    );
    let second_query_second_hop = fallback_info(
        first_route,
        final_route,
        final_route,
        "sticky",
        None,
        "provider-e",
        Some("request-b-to-d-to-e"),
    );
    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: first_route.into(),
            profile: Some("provider-b".into()),
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &second_query_first_hop, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        assert_eq!(
            crate::server_fallback::handle(&orch, &second_query_second_hop, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        crate::server_fallback::flush_pending_notice(&orch)
            .await
            .expect("second query's latest notice flushes");
    })
    .await;

    {
        let session = orch.session.lock().await;
        assert_eq!(session.model, final_route);
        assert_eq!(session.model_profile.as_deref(), Some("provider-e"));
    }

    orch.clear_session().await.expect("clear session");
    let session = orch.session.lock().await;
    assert_eq!(session.model, original);
    assert_eq!(session.model_profile.as_deref(), Some("original-provider"));
}

#[tokio::test]
async fn admitted_server_hop_snapshots_an_intervening_user_model_pick() {
    let source = "claude-opus-4-8";
    let first = fallback_info(
        source,
        "claude-sonnet-4-8",
        "claude-sonnet-4-8",
        "sticky",
        None,
        "sonnet-provider",
        None,
    );
    let later = fallback_info(
        source,
        "claude-haiku-4-8",
        "claude-haiku-4-8",
        "sticky",
        None,
        "haiku-provider",
        None,
    );
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec!["sonnet".into(), "haiku".into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );

    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: None,
        });
        crate::server_fallback::handle(&orch, &first, false)
            .await
            .unwrap();
        lingxi_core::host::OrchestratorHandle::switch_model(&orch, "user-selected-model", None)
            .await
            .unwrap();
        crate::server_fallback::handle(&orch, &later, false)
            .await
            .unwrap();
    })
    .await;

    let session = orch.session.lock().await;
    assert_eq!(session.model, "claude-haiku-4-8");
    assert_eq!(session.model_profile.as_deref(), Some("haiku-provider"));
    drop(session);
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    let latch = selection.live_latch().expect("allowed hop is applied");
    assert_eq!(
        latch.previous_app_state_model,
        Some(Some("user-selected-model".into()))
    );
}

#[tokio::test]
async fn admitted_server_hop_snapshots_an_intervening_provider_pick() {
    let source = "claude-opus-4-8";
    let fallback = fallback_info(
        source,
        "claude-sonnet-4-8",
        "claude-sonnet-4-8",
        "sticky",
        None,
        "resolved-server-profile",
        None,
    );
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec!["sonnet".into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );
    orch.seed_initial_model_profile(source, "original-provider")
        .await;

    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: Some("original-provider".into()),
        });
        lingxi_core::host::OrchestratorHandle::switch_model(
            &orch,
            source,
            Some("user-selected-provider"),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::server_fallback::handle(&orch, &fallback, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
    })
    .await;

    let session = orch.session.lock().await;
    assert_eq!(session.model, "claude-sonnet-4-8");
    assert_eq!(
        session.model_profile.as_deref(),
        Some("resolved-server-profile")
    );
    drop(session);
    let selection = orch.model_runtime.refusal_selection.lock().unwrap();
    let latch = selection.live_latch().expect("allowed hop is applied");
    assert_eq!(
        latch.previous_profile.as_deref(),
        Some("user-selected-provider")
    );
}

#[tokio::test]
async fn implicit_request_profile_accepts_resolved_server_profile() {
    let source = "claude-opus-4-8";
    let fallback = fallback_info(
        source,
        "claude-sonnet-4-8",
        "claude-sonnet-4-8",
        "sticky",
        None,
        "resolved-server-profile",
        None,
    );
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec!["sonnet".into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        Vec::new(),
    );

    crate::server_fallback::scope_query(async {
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: source.into(),
            profile: None,
        });
        assert_eq!(
            crate::server_fallback::handle(&orch, &fallback, false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
    })
    .await;

    let session = orch.session.lock().await;
    assert_eq!(session.model, "claude-sonnet-4-8");
    assert_eq!(
        session.model_profile.as_deref(),
        Some("resolved-server-profile")
    );
}

#[tokio::test]
async fn declined_received_model_discards_text_and_tool_use() {
    let source = "claude-opus-4-8";
    let declared = "claude-opus-4-8";
    let received = "claude-sonnet-4-8";
    let info = fallback_info(
        source,
        received,
        declared,
        "sticky",
        None,
        "first-party-anthropic",
        Some("server-request-2"),
    );
    let mut response = mock_message_response(
        vec![
            LlmContentBlock::Text {
                text: "must not be shown".into(),
                cache_control: None, citations: None,
            },
            LlmContentBlock::ToolCall {
                id: "toolu_disallowed".into(),
                name: "Bash".into(),
                input: json!({"command":"touch should-not-run"}),
            },
        ],
        Some("tool_use"),
    );
    attach_fallback(&mut response, &info);
    let (orch, _, output) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            max_turns: 4,
            server_refusal_fallback: Some(server_lane_policy(declared)),
            server_fallback_model_enforcement: Some(ModelEnforcement::Active {
                allowlist: vec!["opus".into()],
                overrides: BTreeMap::new(),
            }),
            ..Default::default()
        },
        vec![response],
    );

    orch.run_turn("declined turn")
        .await
        .expect("decline is terminal");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert!(!session.history.iter().any(|message| {
        message_contains_text(message, "must not be shown")
            || message.tool_calls().into_iter().any(|tool| {
                matches!(tool, ContentBlock::ToolUse { id, .. }
                    if id.as_str() == "toolu_disallowed")
            })
    }));
    drop(session);
    let text = output.text_events().await.join("");
    assert_eq!(
        text,
        lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR
    );
}

#[test]
fn allowlist_default_exception_uses_received_canonical_identity() {
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            server_fallback_regular_available_models: Some(Vec::new()),
            server_fallback_default_model: Some(" Claude-Sonnet-4-8[2m] ".into()),
            server_fallback_model_enforcement: Some(ModelEnforcement::Inactive),
            ..Default::default()
        },
        Vec::new(),
    );
    assert!(crate::server_fallback::received_model_allowed(
        &orch,
        "claude-sonnet-4-8"
    ));
    assert!(!crate::server_fallback::received_model_allowed(
        &orch,
        "claude-sonnet-4-7"
    ));
}

#[tokio::test]
async fn accepted_fallback_count_is_query_local_and_unclamped() {
    let source = "model-0";
    let mut server_policy = server_lane_policy("model-3");
    server_policy.already_used = true;
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            server_refusal_fallback: Some(server_policy),
            ..Default::default()
        },
        Vec::new(),
    );

    crate::server_fallback::scope_query(async {
        let initial = crate::query_model::fallback_target_context(&orch, source, None).await;
        assert!(!initial.server_fallback.unwrap().query.already_used);
        for (from, to) in [
            ("model-0", "model-1"),
            ("model-1", "model-2"),
            ("model-2", "model-3"),
        ] {
            let info = fallback_info(from, to, to, "sticky", None, "first-party-anthropic", None);
            crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                model: from.into(),
                profile: None,
            });
            assert_eq!(
                crate::server_fallback::handle(&orch, &info, false)
                    .await
                    .unwrap(),
                crate::server_fallback::ServerFallbackAdmission::Applied
            );
        }
        for _ in 0..300 {
            crate::server_fallback::record_event();
        }
        assert_eq!(crate::server_fallback::query_state().fallback_count, 303);
        let context = crate::query_model::fallback_target_context(&orch, "model-3", None).await;
        let query = context.server_fallback.unwrap().query;
        assert!(query.already_used);
        assert!(!query.declined);
    })
    .await;
    assert_eq!(crate::server_fallback::query_state().fallback_count, 0);
}

#[tokio::test]
async fn ordinary_response_without_an_admitted_event_keeps_the_selected_route() {
    let source = "claude-opus-4-8";
    let response = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "ordinary answer".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let (orch, _, _) = orchestrator(
        OrchestratorConfig {
            model: source.into(),
            max_turns: 4,
            ..Default::default()
        },
        vec![response],
    );

    orch.run_turn("ordinary turn").await.expect("ordinary turn");
    let session = orch.session.lock().await;
    assert_eq!(session.model, source);
    assert!(session.model_profile.is_none());
    assert!(session
        .history
        .iter()
        .any(|message| { message_contains_text(message, "ordinary answer") }));
}
