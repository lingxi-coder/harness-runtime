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
