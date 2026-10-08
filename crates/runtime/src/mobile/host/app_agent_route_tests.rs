use super::*;
use agent::model_resolution::{
    ModelProviderKind, ModelResolutionContext, ModelResolutionError, ModelRouteFacts,
};
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    noop_hook_executor, text_delta, MockApiClient, NoOpPermissionGate,
};
use orchestrator::test_support_stream::MockStreamingApiClient;

#[tokio::test]
async fn app_agent_restored_history_keeps_the_admitted_boot_profile_for_shared_model() {
    let root = tempfile::tempdir().unwrap();
    let service = Arc::new(
        local_apps::AppService::load(
            root.path(),
            Arc::new(local_apps::test_support::FixedClock::new(1)),
            Arc::new(local_apps::NoopAppEventObserver),
        )
        .await
        .unwrap(),
    );
    let record = service
        .create_app(Some("Agent route"), "route fixture", None)
        .await
        .unwrap();
    let layout = local_apps::AppLayout::new(root.path(), &record.id).unwrap();
    let session = local_apps::AgentSessionRecord {
        schema_version: local_apps::RUNTIME_CONTRACT_SCHEMA_VERSION,
        session_id: "session-route".into(),
        app_id: record.id.clone(),
        app_instance_id: "instance-route".into(),
        status: local_apps::AgentSessionStatus::Active,
        prompt_profile_revision: 0,
        budget: local_apps::AgentBudget::default(),
        turn_count: 0,
        output_tokens_used: 0,
        bridge_calls_used: 0,
        mcp_calls_used: 0,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    local_apps::save_agent_history(
        &layout,
        &session.session_id,
        &[
            serde_json::to_value(lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "prior app history".into(),
            ))
            .unwrap(),
        ],
    )
    .unwrap();
    let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
    assert!(transport.attach_service(service).is_ok());
    let stream = Arc::new(MockStreamingApiClient::with_turns(vec![
        orchestrator::scripted![
            message_start("app-result", "shared-model"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop()
        ],
    ]));
    let lookup_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let lookups = lookup_count.clone();
    let provider = Arc::new(move |model: &str, profile: Option<&str>| {
        let profile = match profile {
            Some("profile-a") => "profile-a",
            Some("profile-b") => "profile-b",
            _ => {
                return Err(ModelResolutionError::AmbiguousRoute {
                    model: model.into(),
                    profiles: vec!["profile-a".into(), "profile-b".into()],
                })
            }
        };
        lookups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ModelResolutionContext {
            route: ModelRouteFacts {
                model: model.into(),
                profile: Some(profile.into()),
                provider: Some(ModelProviderKind::Other),
                endpoint: Some(format!("https://{profile}.invalid")),
                protocol: Some("OpenAiResponses".into()),
            },
            ..Default::default()
        })
    });
    let config = OrchestratorConfig {
        model: "shared-model".into(),
        ..Default::default()
    };
    let executor = MobileAppAgentExecutor::new(
        config,
        Some("profile-b".into()),
        provider,
        Arc::new(MockApiClient::new(Vec::new())),
        stream.clone(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        root.path().join(branding::DOT_DIR),
        root.path().to_path_buf(),
        transport,
        Arc::new(mcp::projects_session::ProjectsSessionHostContext::new(None)),
        tool_api::test_support::shell_test_ctx_in(
            mobile_linux_api::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
            root.path().to_path_buf(),
        ),
    );
    let (app_agent, _, _) = executor
        .get_or_create_agent(
            &record.id,
            &session.session_id,
            &session,
            Arc::new(AgentTurnUsageState::default()),
        )
        .await
        .unwrap();
    app_agent
        .run_turn_streaming("continue app task")
        .await
        .unwrap();
    let calls = stream.captured_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "shared-model");
    assert_eq!(calls[0].profile.as_deref(), Some("profile-b"));
    assert!(
        lookup_count.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "app queries use the configured route resolver"
    );
    assert!(calls[0]
        .messages
        .iter()
        .any(|message| message.text_content() == "prior app history"));
    let names: Vec<&str> = calls[0]
        .tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(!names.is_empty(), "the app owns its actual MCP catalog");
    assert!(names.iter().all(|name| name.starts_with("mcp__")));
    assert!(!names.contains(&"Skill"));
}
