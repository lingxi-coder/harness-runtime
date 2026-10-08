use super::*;
use tool_api::tool_trait::BashPrecommitSkills;

fn suggestion(case_name: &str) -> String {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../tools/shell/tests/fixtures/bash_precommit_286.json"
    ))
    .unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == case_name)
        .unwrap()["outputs"][0]["suggestion"]
        .as_str()
        .unwrap()
        .to_string()
}

fn description(tools: Vec<serde_json::Value>) -> String {
    tools
        .into_iter()
        .find(|tool| tool["name"] == "Shell")
        .unwrap()["description"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn mobile_boot_reload_and_child_wire_share_custom_precommit_skills() {
    let root = tempfile::tempdir().unwrap();
    let settings_dir = root.path().join(".lingxi");
    std::fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    std::fs::write(&settings_path, r#"{"includeCodeReviewSuggestion":false}"#).unwrap();
    for name in ["verify", "simplify"] {
        let dir = settings_dir.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: Custom {name}\n---\nRun custom {name}.\n"),
        )
        .unwrap();
    }
    let mut cfg = test_config(root.path());
    cfg.enable_mobile_linux_shell();
    let child_selection = agent::ResolvedModelSelection {
        model: cfg.default_model.clone(),
        model_profile: None,
        model_resolution_context: agent::ModelResolutionContext {
            route: agent::ModelRouteFacts {
                model: cfg.default_model.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
    };
    let runtime = build_mobile(
        cfg,
        Arc::new(HostFakePlatform::new(root.path().into())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .await
    .unwrap();
    let custom = BashPrecommitSkills {
        custom_verify: true,
        custom_simplify: true,
        code_review: false,
    };
    assert_eq!(
        runtime
            .wired_skill_listing_provider
            .bash_precommit_skills()
            .await,
        custom
    );
    assert_eq!(
        runtime.mcp_tool_registry.bash_precommit_skills().await,
        custom
    );
    let expected = suggestion("verify_and_simplify");
    assert!(description(runtime.orchestrator.mcp_tool_definitions().await).contains(&expected));

    assert!(matches!(
        runtime.dispatcher.dispatch("/reload-skills").await,
        SlashDispatchResult::Handled { .. }
    ));
    assert_eq!(
        runtime
            .wired_skill_listing_provider
            .bash_precommit_skills()
            .await,
        custom
    );
    for name in ["verify", "simplify"] {
        let descriptor = runtime
            .wired_skill_loader
            .load(name)
            .await
            .unwrap()
            .unwrap();
        assert!(descriptor.body.contains(&format!("Run custom {name}.")));
    }

    let mut definition = agent::builtins::builtin_agent_definitions()
        .into_iter()
        .find(|definition| definition.agent_type == "general-purpose")
        .unwrap();
    let (tools, _) = agent::resolve_subagent_tools(
        &runtime.mcp_tool_registry,
        &definition,
        &[],
        Some(&child_selection),
        0,
        false,
        &[],
    )
    .await
    .unwrap();
    assert!(description(tools).contains(&expected));
    definition.tools = agent::AgentToolPolicy::Except(vec!["Skill".into()]);
    let (tools, _) = agent::resolve_subagent_tools(
        &runtime.mcp_tool_registry,
        &definition,
        &[],
        Some(&child_selection),
        0,
        false,
        &[],
    )
    .await
    .unwrap();
    assert!(!description(tools).contains("right before the `commit`"));

    std::fs::write(&settings_path, r#"{"includeCodeReviewSuggestion":true}"#).unwrap();
    assert!(
        description(runtime.orchestrator.mcp_tool_definitions().await)
            .contains(&suggestion("all_three"))
    );

    runtime
        .slash_registry
        .write()
        .await
        .set_session_skill_allowlist(Some(vec![]));
    assert!(
        !description(runtime.orchestrator.mcp_tool_definitions().await)
            .contains("right before the `commit`")
    );
}

#[test]
fn mobile_code_review_setting_uses_host_root_and_strict_tolerant_bool() {
    let root = tempfile::tempdir().unwrap();
    let cfg = test_config(root.path());
    std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
    let path = cfg.lingxi_home.join("settings.json");
    let provider = super::super::mobile_code_review_suggestion_provider(cfg);
    for (value, expected) in [
        ("true", true),
        ("false", false),
        ("\"true\"", false),
        ("null", false),
    ] {
        std::fs::write(
            &path,
            format!("{{\"includeCodeReviewSuggestion\":{value}}}"),
        )
        .unwrap();
        assert_eq!(provider(), expected, "{value}");
    }
}
