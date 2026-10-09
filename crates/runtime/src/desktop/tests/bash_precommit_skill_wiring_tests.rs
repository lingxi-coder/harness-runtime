use super::*;
use command_api::{CommandFrontmatter, CommandSource, SlashCommand, SlashCommandKind};
use std::sync::atomic::{AtomicBool, Ordering};
use tool_api::tool_trait::BashPrecommitSkills;

fn oracle() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../tools/shell/tests/fixtures/bash_precommit_286.json"
    ))
    .unwrap()
}

fn command(name: &str, origin: &str) -> SlashCommand {
    SlashCommand {
        name: name.into(),
        description: format!("Run {name}"),
        source: if origin == "builtin" {
            CommandSource::Builtin
        } else {
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
        },
        kind: SlashCommandKind::Markdown {
            file_path: PathBuf::from(format!("/repo/.lingxi/skills/{name}/SKILL.md")),
            frontmatter: CommandFrontmatter::default(),
            prompt_template: format!("Run the {name} workflow."),
        },
        loaded_from: Some(origin.into()),
        has_user_specified_description: true,
        ..SlashCommand::default()
    }
}

#[tokio::test]
async fn eligible_skill_provenance_matches_native_precommit_fixture() {
    for case in oracle()["cases"].as_array().unwrap() {
        // Offered-tool availability is checked by the real wire builder below.
        if case["name"] == "skill_tool_absent" {
            continue;
        }
        for (step, output) in case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["outputs"].as_array().unwrap())
        {
            let mut registry = CommandRegistry::new();
            for input in step["commands"].as_array().unwrap() {
                let name = input["name"].as_str().unwrap();
                // The native array's first matching command owns the name.
                if registry.resolve(name).is_none() {
                    registry.register_command(command(name, input["loadedFrom"].as_str().unwrap()));
                }
            }
            let enabled = step["include_code_review_suggestion"] == true;
            let provider = registry_skill_listing_provider(
                Arc::new(RwLock::new(registry)),
                tool_api::read_file_state::new_read_file_state_map(),
                Arc::new(move || enabled),
            );
            assert_eq!(
                provider.bash_precommit_skills().await,
                BashPrecommitSkills {
                    custom_verify: output["skills"]["verify"].as_bool().unwrap(),
                    custom_simplify: output["skills"]["simplify"].as_bool().unwrap(),
                    code_review: enabled && output["skills"]["codeReview"].as_bool().unwrap(),
                },
                "{}",
                case["name"]
            );
        }
    }
}

#[tokio::test]
async fn disabled_and_conditional_skills_do_not_reach_precommit_suggestions() {
    let mut registry = CommandRegistry::new();
    let mut verify = command("verify", "skills");
    verify.disable_model_invocation = true;
    registry.register_command(verify);
    let mut simplify = command("simplify", "skills");
    simplify.paths = Some(vec!["**/*.tsx".into()]);
    registry.register_command(simplify);
    let provider = registry_skill_listing_provider(
        Arc::new(RwLock::new(registry)),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(|| true),
    );
    assert_eq!(
        provider.bash_precommit_skills().await,
        BashPrecommitSkills::default()
    );
}

struct LiveSkillGate(AtomicBool);

#[async_trait::async_trait]
impl lingxi_core::host::PermissionGate for LiveSkillGate {
    async fn check(&self, _: &str, _: &serde_json::Value) -> lingxi_core::host::PermissionDecision {
        lingxi_core::host::PermissionDecision::Allow
    }

    async fn tool_wide_deny_names(&self) -> Vec<String> {
        if self.0.load(Ordering::Relaxed) {
            vec!["Skill".into()]
        } else {
            Vec::new()
        }
    }
}

async fn bash_description(orchestrator: &ConversationOrchestrator) -> String {
    orchestrator
        .mcp_tool_definitions()
        .await
        .into_iter()
        .find(|tool| tool["name"] == "Bash")
        .unwrap()["description"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn actual_bash_wire_description_tracks_reload_settings_and_offered_skill_tool() {
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
    };
    struct RestorePrecommitFlag;
    impl Drop for RestorePrecommitFlag {
        fn drop(&mut self) {
            telemetry::test_clear_flag("tengu_polished_tulip");
        }
    }
    let _restore_flag = RestorePrecommitFlag;
    telemetry::test_set_flag("tengu_polished_tulip", true);
    let ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    let registry = Arc::new(RwLock::new(CommandRegistry::new()));
    registry
        .write()
        .await
        .register_command(command("verify", "bundled"));
    registry
        .write()
        .await
        .register_command(command("simplify", "bundled"));
    registry
        .write()
        .await
        .register_command(command("code-review", "bundled"));
    let code_review_enabled = Arc::new(AtomicBool::new(false));
    let enabled = code_review_enabled.clone();
    let provider = registry_skill_listing_provider(
        registry.clone(),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(move || enabled.load(Ordering::Relaxed)),
    );
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(tool_shell::BashTool::new(ctx.clone())));
    tools.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
        ctx,
        Arc::new(skill_loader::CommandRegistrySkillLoader::new(
            registry.clone(),
        )),
    )));
    let gate = Arc::new(LiveSkillGate(AtomicBool::new(false)));
    let orchestrator = ConversationOrchestrator::new(
        orchestrator::OrchestratorConfig {
            model: "claude-opus-4-8".into(),
            ..Default::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        noop_hook_executor(),
        gate.clone(),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/repo"),
    )
    .with_skill_listing(provider);

    assert!(!bash_description(&orchestrator)
        .await
        .contains("right before the `commit`"));
    // The first description already latched true, even with no custom skills.
    // Changing the underlying flag within this session must not rewrite it.
    telemetry::test_set_flag("tengu_polished_tulip", false);
    registry
        .write()
        .await
        .register_command(command("verify", "skills"));
    registry
        .write()
        .await
        .register_command(command("simplify", "commands_DEPRECATED"));
    let fixture = oracle();
    let expected = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "verify_and_simplify")
        .unwrap()["outputs"][0]["suggestion"]
        .as_str()
        .unwrap();
    assert!(bash_description(&orchestrator).await.contains(expected));

    // The wire cache must invalidate even though the offered tool names/model did not change.
    code_review_enabled.store(true, Ordering::Relaxed);
    let expected = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "all_three")
        .unwrap()["outputs"][0]["suggestion"]
        .as_str()
        .unwrap();
    assert!(bash_description(&orchestrator).await.contains(expected));

    gate.0.store(true, Ordering::Relaxed);
    assert!(!bash_description(&orchestrator)
        .await
        .contains("right before the `commit`"));
    gate.0.store(false, Ordering::Relaxed);
    assert!(bash_description(&orchestrator).await.contains(expected));

    registry
        .write()
        .await
        .set_session_skill_allowlist(Some(Vec::new()));
    assert!(!bash_description(&orchestrator)
        .await
        .contains("right before the `commit`"));
    registry.write().await.set_session_skill_allowlist(None);
    assert!(bash_description(&orchestrator).await.contains(expected));

    // Use the real session-switch seams: the registry and Bash object survive,
    // while clear and hot resume must start a new wording decision and cache.
    lingxi_core::host::OrchestratorHandle::clear_session(&orchestrator)
        .await
        .expect("clear session with the same tool registry");
    assert!(
        !bash_description(&orchestrator)
            .await
            .contains("right before the `commit`"),
        "/clear must resolve the now-disabled flag again"
    );
    telemetry::test_set_flag("tengu_polished_tulip", true);
    assert!(
        !bash_description(&orchestrator)
            .await
            .contains("right before the `commit`"),
        "the cleared session must retain its false decision"
    );
    lingxi_core::host::OrchestratorHandle::resume_session(
        &orchestrator,
        lingxi_core::types::SessionId::new(),
        Vec::new(),
        None,
        None,
        lingxi_core::host::ResumeRuntimeSnapshot::default(),
    )
    .await
    .expect("hot resume another root session with the same tool registry");
    assert!(
        bash_description(&orchestrator).await.contains(expected),
        "hot resume must resolve the now-enabled flag again"
    );
    telemetry::test_set_flag("tengu_polished_tulip", false);
    assert!(
        bash_description(&orchestrator).await.contains(expected),
        "the resumed session must retain its true decision"
    );
}

#[test]
fn code_review_suggestion_uses_live_owned_settings_and_session_cwd() {
    let (_tmp, mut cfg) = tests::test_config(true);
    cfg.lingxi_home = cfg.cwd.join("user-config");
    std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
    let settings = cfg.lingxi_home.join("settings.json");
    let cwd = SessionCwd::new(cfg.cwd.clone(), vec![]);
    let provider = code_review_suggestion_provider(cfg.clone(), cwd.clone());
    std::fs::write(&settings, r#"{"includeCodeReviewSuggestion":false}"#).unwrap();
    assert!(!provider());
    std::fs::write(&settings, r#"{"includeCodeReviewSuggestion":true}"#).unwrap();
    assert!(provider());

    let next_cwd = cfg.cwd.join("next-project");
    std::fs::create_dir_all(next_cwd.join(".lingxi")).unwrap();
    std::fs::write(
        next_cwd.join(".lingxi/settings.json"),
        r#"{"includeCodeReviewSuggestion":false}"#,
    )
    .unwrap();
    cwd.swap(next_cwd, vec![]);
    assert!(!provider());
}
