use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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

fn description_from_projections(tools: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    let tool = tools
        .into_iter()
        .find(|tool| tool.string_at("/name") == Some("Shell"))
        .unwrap();
    let units = tool
        .string_units("/description")
        .expect("Shell description is a source string");
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display is derived from the exact source units")
}

fn description_from_source_tools(
    tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
) -> Utf16JsonProjection {
    tools
        .into_iter()
        .find(|tool| tool.name == "Shell")
        .unwrap()
        .description
}

fn source_contains(value: &Utf16JsonProjection, needle: &str) -> bool {
    let Some(units) = value.string_units("") else {
        return false;
    };
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    !needle.is_empty()
        && units
            .windows(needle.len())
            .any(|candidate| candidate == needle.as_slice())
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
    assert!(source_contains(
        &description_from_projections(runtime.orchestrator.mcp_tool_definitions().await),
        &expected
    ));

    assert!(matches!(
        runtime
            .dispatcher
            .dispatch(Utf16JsonProjection::from("/reload-skills"))
            .await,
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
            .load(&Utf16JsonProjection::from(name))
            .await
            .unwrap()
            .unwrap();
        assert!(source_contains(
            &descriptor.body,
            &format!("Run custom {name}.")
        ));
    }

    let mut definition = agent::builtins::builtin_agent_definitions()
        .into_iter()
        .find(|definition| definition.agent_type == "general-purpose")
        .unwrap();
    let (tools, _) = agent::resolve_subagent_tools(
        &runtime.mcp_tool_registry,
        &definition,
        &[],
        None,
        0,
        false,
        &[],
    )
    .await
    .unwrap();
    assert!(source_contains(
        &description_from_source_tools(tools),
        &expected
    ));
    definition.tools = agent::AgentToolPolicy::Except(vec!["Skill".into()]);
    let (tools, _) = agent::resolve_subagent_tools(
        &runtime.mcp_tool_registry,
        &definition,
        &[],
        None,
        0,
        false,
        &[],
    )
    .await
    .unwrap();
    assert!(!source_contains(
        &description_from_source_tools(tools),
        "right before the `commit`"
    ));

    std::fs::write(&settings_path, r#"{"includeCodeReviewSuggestion":true}"#).unwrap();
    assert!(source_contains(
        &description_from_projections(runtime.orchestrator.mcp_tool_definitions().await),
        &suggestion("all_three")
    ));

    runtime
        .slash_registry
        .write()
        .await
        .set_session_skill_allowlist(Some(vec![]));
    assert!(!source_contains(
        &description_from_projections(runtime.orchestrator.mcp_tool_definitions().await),
        "right before the `commit`"
    ));
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
