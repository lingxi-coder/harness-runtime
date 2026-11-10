use super::*;
use lingxi_core::host::prompt_policy::{
    PromptBundle, PromptModelSelector, PromptRegistry, PromptRule, PromptTemplateOverride,
    ReminderPromptBinding, ToolPromptBinding,
};
use lingxi_core::host::session_context::ConversationMode;

fn context(model: &str) -> SystemPromptContext {
    SystemPromptContext {
        prompt_policy: None,
        native_system_turns: None,
        cwd: PathBuf::from("/workspace"),
        platform: "darwin".into(),
        model: model.into(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        shell: "zsh".into(),
        os_version: "Darwin 27.2.0".into(),
        git_status: None,
        in_worktree: false,
        file_tree: FileTree::default(),
        memory_files: Vec::new(),
        tool_names: vec!["Read".into(), "Write".into()],
        skills_available: false,
        is_interactive: false,
        memory_dir: None,
        exclude_dynamic_sections: false,
    }
}

fn routed(mode: ConversationMode, provider: &str, model: &str) -> Arc<ResolvedPromptPolicy> {
    Arc::new(resolve_prompt_policy(PromptTarget::from_route(
        mode,
        provider,
        model,
        None,
        None,
        Some("test-connection"),
    )))
}

#[test]
fn generic_chat_and_code_use_distinct_provider_neutral_templates() {
    let mut ctx = context("gpt-5.5");
    ctx.prompt_policy = Some(routed(ConversationMode::Chat, "openai", &ctx.model));
    let chat = assemble_system_prompt(&ctx);
    assert!(chat.contains("research, writing, learning"));
    assert!(!chat.contains("# Working with the user"));
    assert!(prompt_policy_is_lean(ctx.prompt_policy.as_ref().unwrap()));

    ctx.prompt_policy = Some(routed(ConversationMode::Code, "openai", &ctx.model));
    let code = assemble_system_prompt(&ctx);
    assert!(code.contains("# Working with the user"));
    assert_ne!(chat, code);
    for body in [chat, code] {
        assert!(!body.contains("Claude"));
        assert!(!body.contains("# Environment"));
        assert!(!body.contains("# Delivering work"));
        assert!(!body.contains("# auto memory"));
        assert!(!body.contains("mid-conversation system turns"));
    }
}

#[test]
fn preview_unknown_provider_defaults_to_lean_and_does_not_guess_claude() {
    for model in ["new-model-v9", "vendor-compat-claude-sonnet-5", "kimi-k2.5"] {
        let ctx = context(model);
        let sections = named_prompt_sections_for_mode(&ctx, None, ConversationMode::Code);
        let policy = resolve_context_prompt_policy(&ctx, ConversationMode::Code);
        assert_eq!(policy.bundle_id, "generic-code-lean-v1");
        assert!(prompt_policy_is_lean(&policy));
        let slots = prompt_section_slots(sections, &ctx);
        assert!(slots.iter().any(|slot| slot.id == "lean_body"));
        assert!(!slots.iter().any(|slot| slot.id == "fable_identity"));
    }
}

#[test]
fn anthropic_protocol_provider_does_not_imply_a_claude_model() {
    let mut ctx = context("kimi-k2.5");
    ctx.prompt_policy = Some(routed(
        ConversationMode::Code,
        "kimi-code-anthropic",
        &ctx.model,
    ));
    let policy = ctx.prompt_policy.as_ref().unwrap();
    assert!(!policy.tools.is_native());
    assert!(assemble_system_prompt(&ctx).contains("# Tools and actions"));
    assert!(!assemble_system_prompt(&ctx).contains("Claude"));
}

#[test]
fn openrouter_claude_alias_uses_the_underlying_native_renderer() {
    let canonical = context("claude-opus-5");
    let mut alias = context("friendly-model-alias");
    alias.prompt_policy = Some(Arc::new(resolve_prompt_policy(PromptTarget::from_route(
        ConversationMode::Code,
        "openrouter",
        &alias.model,
        Some("anthropic/claude-opus-5"),
        None,
        Some("router-account"),
    ))));
    assert_eq!(
        alias.prompt_policy.as_ref().unwrap().bundle_id,
        "claude-code"
    );
    assert_eq!(
        assemble_system_prompt(&alias),
        assemble_system_prompt(&canonical)
    );
}

#[test]
fn frozen_policy_mode_wins_over_a_stale_mode_argument() {
    let mut ctx = context("gpt-5.5");
    ctx.prompt_policy = Some(routed(ConversationMode::Chat, "openai", &ctx.model));
    let actual = render_prompt_sections(&named_prompt_sections_for_mode(
        &ctx,
        None,
        ConversationMode::Code,
    ));
    assert_eq!(actual, assemble_system_prompt(&ctx));
    assert!(actual.starts_with(profile_header(ConversationMode::Chat)));
}

#[test]
fn actual_route_system_turn_support_overrides_model_defaults() {
    let mut ctx = context("claude-opus-5");
    let policy = routed(ConversationMode::Code, "anthropic", &ctx.model);
    ctx.prompt_policy = Some(Arc::clone(&policy));
    ctx.native_system_turns = Some(false);
    assert!(!body_render_options(&ctx, &policy).explains_system_turns);
    assert!(assemble_system_prompt(&ctx).contains("`<system-reminder>` tags"));
    ctx.native_system_turns = Some(true);
    assert!(body_render_options(&ctx, &policy).explains_system_turns);
    assert!(assemble_system_prompt(&ctx).contains("The system may send updates"));
    for model in ["claude-sonnet-5", "claude-opus-4-8"] {
        ctx.model = model.into();
        let policy = routed(ConversationMode::Code, "anthropic", model);
        ctx.prompt_policy = Some(Arc::clone(&policy));
        assert!(!body_render_options(&ctx, &policy).explains_system_turns);
    }
}

#[test]
fn generic_memory_uses_its_portable_protocol_and_actual_directory() {
    let mut ctx = context("qwen3-coder");
    ctx.memory_dir = Some(PathBuf::from("/persistent/user-memory"));
    let output = assemble_system_prompt(&ctx);
    assert!(output.contains("# Memory"));
    assert!(output.contains("/persistent/user-memory"));
    assert!(output.contains("MEMORY.md"));
    assert!(!output.contains("{memory_path}"));
    assert!(!output.contains("# auto memory"));
}

#[test]
fn registered_future_bundle_replaces_system_memory_and_reminder_components() {
    static BUNDLES: &[PromptBundle] = &[
        PromptBundle {
            id: "future-model",
            version: "2026.11",
            system: SystemPromptBinding::Template("FUTURE_SYSTEM_ONLY"),
            tools: ToolPromptBinding::Lean { overrides: &[] },
            memory: MemoryPromptBinding::Template("FUTURE_MEMORY={memory_path}"),
            reminders: ReminderPromptBinding::Portable {
                base: &[],
                overrides: &[PromptTemplateOverride {
                    key: "context_management",
                    template: "FUTURE_CONTEXT_ONLY",
                }],
            },
        },
        PromptBundle {
            id: "fallback",
            version: "1",
            system: SystemPromptBinding::Template("FALLBACK_BODY"),
            tools: ToolPromptBinding::Lean { overrides: &[] },
            memory: MemoryPromptBinding::Template("FALLBACK_MEMORY"),
            reminders: ReminderPromptBinding::Portable {
                base: &[],
                overrides: &[],
            },
        },
    ];
    static RULES: &[PromptRule] = &[
        PromptRule {
            id: "future-exact",
            mode: ConversationMode::Code,
            provider_id: Some("future-provider"),
            model: PromptModelSelector::Exact("future-v9"),
            bundle_id: "future-model",
        },
        PromptRule {
            id: "code-fallback",
            mode: ConversationMode::Code,
            provider_id: None,
            model: PromptModelSelector::Any,
            bundle_id: "fallback",
        },
        PromptRule {
            id: "chat-fallback",
            mode: ConversationMode::Chat,
            provider_id: None,
            model: PromptModelSelector::Any,
            bundle_id: "fallback",
        },
    ];
    let registry = PromptRegistry::new(BUNDLES, RULES).unwrap();
    let mut ctx = context("future-v9");
    ctx.memory_dir = Some(PathBuf::from("/future-memory"));
    ctx.prompt_policy = Some(Arc::new(
        registry
            .resolve(PromptTarget::from_route(
                ConversationMode::Code,
                "future-provider",
                &ctx.model,
                None,
                None,
                None,
            ))
            .unwrap(),
    ));
    let output = assemble_system_prompt(&ctx);
    assert!(output.contains("FUTURE_SYSTEM_ONLY"));
    assert!(output.contains("FUTURE_MEMORY=/future-memory"));
    assert!(output.contains("FUTURE_CONTEXT_ONLY"));
    assert!(!output.contains("FALLBACK_BODY"));
    assert!(!output.contains("# Working with the user"));
    assert!(!output.contains("# auto memory"));
    assert_eq!(
        ctx.prompt_policy.as_ref().unwrap().matched_rule,
        "future-exact"
    );
}

#[test]
fn prompt_product_environment_controls_and_alias_precedence() {
    use lingxi_core::host::model_capabilities as capabilities;
    const MARKER: &str = "LINGXI_PROMPT_REVIEW_ENV_CASE";
    if let Ok(case) = std::env::var(MARKER) {
        let enabled = case == "enabled";
        assert_eq!(
            capabilities::claude_lean_prompt_for("claude-sonnet-4-6"),
            enabled
        );
        assert_eq!(capabilities::lean_rewrite_for("claude-sonnet-4-6"), enabled);
        assert_eq!(
            capabilities::prompt_system_turns_for("claude-opus-5"),
            enabled
        );
        assert_eq!(
            body_sections::is_communicating_model("claude-opus-5"),
            enabled
        );
        assert_eq!(silent_turn::is_enabled("claude-opus-5"), enabled);
        assert_eq!(
            total_tokens::resolve_mode_for_model("claude-opus-5", None)
                != total_tokens::TotalTokensMode::Off,
            enabled
        );
        return;
    }
    let allow = "lean_prompt,temporal_hare,mid_conv_system,fable_5_mitigations,silent_turn_reminder,total_tokens_reminder";
    let deny = "-lean_prompt,-temporal_hare,-mid_conv_system,-fable_5_mitigations,-silent_turn_reminder,-total_tokens_reminder";
    for (product, legacy, expected) in [
        (Some(allow), None, "enabled"),
        (None, Some(allow), "enabled"),
        (Some(deny), Some(allow), "disabled"),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "prompt::policy_dispatch_tests::prompt_product_environment_controls_and_alias_precedence", "--nocapture"])
            .env(MARKER, expected);
        for key in [
            branding::MODEL_CAPABILITIES_ENV,
            "CLAUDE_CODE_MODEL_CAPABILITIES",
            "LINGXI_SIMPLE_SYSTEM_PROMPT",
            "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
            "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
            "CLAUDE_CODE_SILENT_TURN_REMINDER",
            "CLAUDE_CODE_TOTAL_TOKENS_REMINDER",
        ] {
            command.env_remove(key);
        }
        if let Some(value) = product {
            command.env(branding::MODEL_CAPABILITIES_ENV, value);
        }
        if let Some(value) = legacy {
            command.env("CLAUDE_CODE_MODEL_CAPABILITIES", value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn prompt_budget_obeys_product_attachment_gates() {
    const MARKER: &str = "LINGXI_PROMPT_REVIEW_BUDGET_CASE";
    if let Ok(case) = std::env::var(MARKER) {
        let enabled = case == "enabled";
        let body = assemble_system_prompt(&context("claude-opus-5"));
        assert_eq!(body.contains("<total_tokens>"), enabled, "{body}");
        return;
    }
    for (product, legacy) in [
        (
            branding::DISABLE_ATTACHMENTS_ENV,
            "CLAUDE_CODE_DISABLE_ATTACHMENTS",
        ),
        (branding::SIMPLE_ENV, "CLAUDE_CODE_SIMPLE"),
    ] {
        for value in ["1", "false", ""] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "prompt::policy_dispatch_tests::prompt_budget_obeys_product_attachment_gates",
                    "--nocapture",
                ])
                .env(
                    MARKER,
                    if value.is_empty() {
                        "enabled"
                    } else {
                        "disabled"
                    },
                );
            for key in [
                branding::DISABLE_ATTACHMENTS_ENV,
                branding::SIMPLE_ENV,
                "CLAUDE_CODE_DISABLE_ATTACHMENTS",
                "CLAUDE_CODE_SIMPLE",
                branding::MODEL_CAPABILITIES_ENV,
                "CLAUDE_CODE_MODEL_CAPABILITIES",
                "CLAUDE_CODE_TOTAL_TOKENS_REMINDER",
            ] {
                command.env_remove(key);
            }
            command.env(product, value).env(legacy, "1");
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}

#[test]
fn prompt_generic_background_constraints_reach_chat_and_code_bodies() {
    const MARKER: &str = "LINGXI_PROMPT_REVIEW_BACKGROUND";
    if std::env::var_os(MARKER).is_some() {
        for (model, provider) in [("gpt-5.5", "openai"), ("kimi-k2.5", "kimi")] {
            for mode in [ConversationMode::Chat, ConversationMode::Code] {
                let mut ctx = context(model);
                ctx.prompt_policy = Some(routed(mode, provider, model));
                ctx.tool_names = vec![
                    "Read".into(),
                    "Write".into(),
                    "EnterWorktree".into(),
                    "Bash".into(),
                ];
                let body = assemble_system_prompt(&ctx);
                for marker in [
                    "# Background Session",
                    "/jobs/review/tmp",
                    "somewhere durable",
                    "Call EnterWorktree as your first action",
                    "Never push to main/master, force-push, or merge.",
                ] {
                    assert!(body.contains(marker), "{model} {mode:?}: {marker}");
                }
                assert!(!body.contains("{isolation_guidance}"));
            }
        }
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "prompt::policy_dispatch_tests::prompt_generic_background_constraints_reach_chat_and_code_bodies", "--nocapture"])
        .env(MARKER, "1").env("LINGXI_SESSION_KIND", "bg")
        .env("LINGXI_JOB_DIR", "/jobs/review").env("LINGXI_BG_ISOLATION", "worktree")
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
