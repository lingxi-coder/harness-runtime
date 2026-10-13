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
        description: format!("Run {name}").into(),
        source: if origin == "builtin" {
            CommandSource::Builtin
        } else {
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
        },
        kind: SlashCommandKind::Markdown {
            file_path: PathBuf::from(format!(
                "/repo/{}/skills/{name}/SKILL.md",
                branding::DOT_DIR
            )),
            frontmatter: CommandFrontmatter::default(),
            prompt_template: format!("Run the {name} workflow.").into(),
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
                if registry.resolve(&name.into()).is_none() {
                    registry.register_command(command(name, input["loadedFrom"].as_str().unwrap()));
                }
            }
            let enabled = step["include_code_review_suggestion"] == true;
            let mut native_settings = super::NativeSkillListingSettings::default();
            if case["name"] == "all_three" {
                // The 2.1.286 fixture predates `rQ`. Model the concrete 2.1.295
                // source case where PolicyVerdictState exists but
                // rememberedCoreSkills is nullish: Native then uses its fixed
                // remembered-core catalog, which includes code-review. This
                // keeps the builtin gate active for unremembered skills while
                // exercising the source-backed allow path for this fixture.
                native_settings.policy_remembered_core_skills = Some(None);
            }
            let settings_loader = Arc::new(move || native_settings.clone());
            let provider = super::registry_skill_listing_provider_with_sources(
                Arc::new(RwLock::new(registry)),
                tool_api::read_file_state::new_read_file_state_map(),
                Arc::new(move || enabled),
                settings_loader,
                Arc::new(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
                None,
            );
            let snapshot = provider.skill_listing_snapshot().await;
            assert_eq!(
                provider.bash_precommit_skills(&snapshot.entries).await,
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
    let snapshot = provider.skill_listing_snapshot().await;
    assert_eq!(
        provider.bash_precommit_skills(&snapshot.entries).await,
        BashPrecommitSkills::default()
    );
}

#[tokio::test]
async fn desktop_skill_listing_uses_native_catalog_overrides_and_usage_snapshot() {
    use orchestrator::prompt::skill_listing::SkillListingBudgetSettings;

    let home = tempfile::tempdir().expect("isolated skill usage home");
    command_api::skill_usage::record_skill_usage(home.path(), &"verify".into())
        .expect("record one canonical skill use");

    let mut verify = command("verify", "skills");
    verify.native_type = Some(command_api::NativeCommandType::Prompt);
    verify.user_facing_name = Some("review".into());
    verify.aliases = vec!["review".into()];
    // Native applies `is_enabled` before building the alias-resolution
    // catalog. This disabled exact-name collision must not suppress verify's
    // valid display alias.
    let mut disabled_collision = command("review", "skills");
    disabled_collision.native_type = Some(command_api::NativeCommandType::Prompt);
    disabled_collision.is_enabled = false;
    // The full catalog is broader than the model-invocable prompt list. An
    // enabled manual-only command still participates in `Rwt` name
    // resolution, even though it cannot become a listing entry.
    let mut manual_name_owner = command("manual-owner", "skills");
    manual_name_owner.user_facing_name = Some("short-name".into());
    manual_name_owner.disable_model_invocation = true;
    let mut short_name_entry = command("short-entry", "skills");
    short_name_entry.native_type = Some(command_api::NativeCommandType::Prompt);
    short_name_entry.user_facing_name = Some("short-name".into());
    verify.unqualified_name = Some("short-verify".into());
    let mut hidden = command("hidden", "skills");
    hidden.native_type = Some(command_api::NativeCommandType::Prompt);
    let mut ordinary = command("ordinary", "skills");
    ordinary.native_type = Some(command_api::NativeCommandType::Prompt);
    let mut builtin = command("builtin-prompt", "skills");
    builtin.source = CommandSource::Builtin;
    builtin.native_type = Some(command_api::NativeCommandType::Prompt);
    let mut plugin = command("plugin-off", "plugin");
    plugin.source = CommandSource::Plugin;
    plugin.native_type = Some(command_api::NativeCommandType::Prompt);
    plugin.native_source = Some("plugin".into());
    plugin.plugin_name = Some("fixture-plugin".into());

    let mut registry = CommandRegistry::new();
    registry.register_command(verify);
    registry.register_command(disabled_collision);
    registry.register_command(manual_name_owner);
    registry.register_command(short_name_entry);
    registry.register_command(hidden);
    registry.register_command(ordinary);
    registry.register_command(builtin);
    registry.register_command(plugin);

    let parsed: lingxi_core::settings::SettingsJson = serde_json::from_str(
        r#"{"skillOverrides":{"verify":"name-only","hidden":"off","plugin-off":"off"}}"#,
    )
    .expect("valid native skill override settings");
    let settings = super::NativeSkillListingSettings {
        base: parsed.skill_overrides,
        policy: None,
        flag: None,
        disable_bundled_skills: false,
        policy_remembered_core_skills: None,
        budget: SkillListingBudgetSettings {
            budget_fraction: Some(0.12),
            max_desc_chars: Some(32.0),
        },
    };
    let root = home.path().to_path_buf();
    let provider = super::registry_skill_listing_provider_with_sources(
        Arc::new(RwLock::new(registry)),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(|| false),
        Arc::new(move || settings.clone()),
        Arc::new(move || root.clone()),
        Some(home.path().to_path_buf()),
    );

    let snapshot = provider.skill_listing_snapshot().await;
    let entries = &snapshot.entries;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.name.string_at("").unwrap())
            .collect::<Vec<_>>(),
        vec!["verify", "short-entry", "ordinary", "plugin-off"]
    );
    assert_eq!(
        entries[0].display_name.string_at(""),
        Some("verify (review)")
    );
    assert_eq!(
        entries[1].display_name.string_at(""),
        Some("short-entry"),
        "enabled manual-only commands remain in Native's full name-resolution catalog"
    );
    assert!(entries[0].is_name_only);
    assert!(entries[0].priority > 0.0);
    let plugin_entry = entries
        .iter()
        .find(|entry| entry.name.string_at("") == Some("plugin-off"))
        .expect("plugin prompt remains listed despite its prompt override");
    assert_eq!(
        plugin_entry
            .source
            .as_ref()
            .and_then(|value| value.string_at("")),
        Some("plugin")
    );
    assert_eq!(
        plugin_entry
            .plugin_name
            .as_ref()
            .and_then(|value| value.string_at("")),
        Some("fixture-plugin")
    );
    assert_eq!(
        snapshot.budget_settings,
        SkillListingBudgetSettings {
            budget_fraction: Some(0.12),
            max_desc_chars: Some(32.0),
        }
    );
}
struct LiveSkillGate(AtomicBool);

#[async_trait::async_trait]
impl lingxi_core::host::PermissionGate for LiveSkillGate {
    async fn check(
        &self,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::host::PermissionDecision {
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
    let tools = orchestrator.mcp_tool_definitions().await;
    tools
        .iter()
        .find(|tool| tool.string_at("/name") == Some("Bash"))
        .and_then(|tool| tool.string_at("/description"))
        .expect("Bash tool description is advertised")
        .to_owned()
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

#[tokio::test]
async fn code_review_suggestion_uses_live_owned_settings_and_session_cwd() {
    let (tmp, mut cfg) = tests::test_config(true);
    cfg.lingxi_home = cfg.cwd.join("user-config");
    std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
    let managed_dir = tmp.path().join("managed");
    std::fs::create_dir_all(&managed_dir).unwrap();
    cfg.managed_policy_snapshot_cache
        .replace(settings_watch::read_managed_settings_snapshot(&managed_dir).await);
    let settings = cfg.lingxi_home.join("settings.json");
    let cwd = SessionCwd::new(cfg.cwd.clone(), vec![]);
    let provider = code_review_suggestion_provider(cfg.clone(), cwd.clone());
    std::fs::write(&settings, r#"{"includeCodeReviewSuggestion":false}"#).unwrap();
    assert!(!provider());
    std::fs::write(&settings, r#"{"includeCodeReviewSuggestion":true}"#).unwrap();
    assert!(provider());

    let next_cwd = cfg.cwd.join("next-project");
    std::fs::create_dir_all(next_cwd.join(branding::DOT_DIR)).unwrap();
    std::fs::write(
        next_cwd.join(branding::DOT_DIR).join("settings.json"),
        r#"{"includeCodeReviewSuggestion":false}"#,
    )
    .unwrap();
    cwd.swap(next_cwd, vec![]);
    assert!(!provider());
}

#[tokio::test]
async fn native_295_skill_description_lines_reach_outbound_messages_unchanged() {
    use llm_runtime::ContentBlock as LlmContentBlock;
    use orchestrator::test_support::{
        mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };

    // These are the four input files and raw Native line slices. The Native
    // capture verified that each slice is the exact UTF-8 representation of
    // the target line in its JSON-string token. The assertion below is at the
    // orchestrator -> API-client boundary, before provider HTTP serialization.
    let cases: [(&str, &[u8], &[u8]); 4] = [
        (
            "ascii-250",
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/ascii-250/SKILL.md.raw"
            ),
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/ascii-250/skill-listing-line.raw"
            ),
        ),
        (
            "unicode-250",
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/unicode-250/SKILL.md.raw"
            ),
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/unicode-250/skill-listing-line.raw"
            ),
        ),
        (
            "ascii-1200",
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/ascii-1200/SKILL.md.raw"
            ),
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/ascii-1200/skill-listing-line.raw"
            ),
        ),
        (
            "lone-surrogate-escape",
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/lone-surrogate-escape/SKILL.md.raw"
            ),
            include_bytes!(
                "../../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/lone-surrogate-escape/skill-listing-line.raw"
            ),
        ),
    ];
    let target_prefix: Vec<u16> = "- native-width-probe: ".encode_utf16().collect();

    for (case, skill_markdown, native_line) in cases {
        let temporary = tempfile::tempdir().expect("isolated skill roots");
        let home = temporary.path().join("home");
        let cwd = home.join("project");
        let lingxi_home = temporary.path().join("config");
        let skill_path = cwd
            .join(branding::DOT_DIR)
            .join("skills/native-width-probe/SKILL.md");
        std::fs::create_dir_all(skill_path.parent().unwrap()).expect("skill directory");
        std::fs::create_dir_all(&home).expect("home directory");
        std::fs::create_dir_all(&lingxi_home).expect("user config directory");
        std::fs::write(&skill_path, skill_markdown).expect("write captured SKILL.md bytes");

        let mut command_registry = CommandRegistry::new();
        let registered = command_api::builtins::load_and_register_skill_commands_with_roots(
            &mut command_registry,
            &cwd,
            &lingxi_home,
            None,
            &home,
            &[],
        )
        .await;
        assert_eq!(
            registered, 1,
            "{case}: disk loader should register one skill"
        );

        let command_registry = Arc::new(RwLock::new(command_registry));
        let listing_provider = registry_skill_listing_provider(
            command_registry.clone(),
            tool_api::read_file_state::new_read_file_state_map(),
            Arc::new(|| false),
        );

        let context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
            context,
            Arc::new(skill_loader::CommandRegistrySkillLoader::new(
                command_registry,
            )),
        )));

        let api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![LlmContentBlock::Text {
                text: "fixed skill-width response".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let orchestrator = ConversationOrchestrator::new(
            orchestrator::OrchestratorConfig {
                model: "claude-opus-4-8".into(),
                ..Default::default()
            },
            api.clone(),
            Arc::new(tools),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::with_files(vec![])),
            cwd,
        )
        .with_skill_listing(listing_provider);

        orchestrator
            .run_turn("List the available skills.")
            .await
            .unwrap_or_else(|error| panic!("{case}: model turn failed: {error}"));

        let captured_calls = api.captured_msgs().await;
        assert_eq!(captured_calls.len(), 1, "{case}: one outbound API call");
        let mut target_lines = Vec::new();
        for message in captured_calls.iter().flatten() {
            let lingxi_core::types::ConversationMessage::User { content, .. } = message else {
                continue;
            };
            for block in content {
                let lingxi_core::types::ContentBlock::TextJsUtf16 {
                    text,
                    utf16_code_units,
                    ..
                } = block
                else {
                    continue;
                };
                for line in utf16_code_units.split(|unit| *unit == b'\n' as u16) {
                    if line.starts_with(&target_prefix) {
                        // These four Native target lines contain only valid
                        // scalar values (the D800 input falls back to ASCII).
                        // Strict decoding makes the UTF-8 byte comparison
                        // honest and avoids any lossy normalization.
                        let line_text = String::from_utf16(line)
                            .expect("target listing line has valid UTF-16 scalar text");
                        assert!(text.contains(&line_text));
                        target_lines.push(line_text);
                    }
                }
            }
        }
        assert_eq!(target_lines.len(), 1, "{case}: one listed target line");
        assert_eq!(
            target_lines[0].as_bytes(),
            native_line,
            "{case}: exact line bytes"
        );
    }
}
