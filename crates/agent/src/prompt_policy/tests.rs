use super::*;
use lingxi_core::host::prompt_policy::PromptTarget;

struct Routes;

#[async_trait::async_trait]
impl SubagentApiClient for Routes {
    fn prompt_target(
        &self,
        model: &str,
        profile: Option<&str>,
        mode: ConversationMode,
    ) -> Result<PromptTarget, llm_runtime::LlmError> {
        let (provider, underlying) = match profile {
            Some("claude-channel") => ("openrouter", "claude-opus-5"),
            Some("kimi-messages") => ("kimi", "kimi-k2.5"),
            _ => ("anthropic", model),
        };
        Ok(PromptTarget::from_route(
            mode,
            provider,
            model,
            Some(underlying),
            None,
            profile,
        ))
    }

    fn supports_system_turns(&self, _model: &str, profile: Option<&str>) -> bool {
        profile != Some("kimi-messages")
    }

    async fn stream(
        &self,
        _: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        unreachable!("route-only fixture")
    }
}

fn state(target: &SubagentPromptTarget) -> SubagentPromptState {
    SubagentPromptState {
        initial_policy: target.policy.clone(),
        role: Some("CUSTOM ROLE".into()),
        fork_prefix: None,
        fork_policy: None,
        custom_system_prompt: None,
        append_system_prompt: None,
        environment: None,
        cwd: None,
        addendum: None,
        operator_suffix: None,
        tool_registry: None,
        system_renderer: None,
        seeded_context: Default::default(),
        replace_initial_context: false,
    }
}

#[test]
fn child_route_disambiguates_same_named_models_and_inherits_mode() {
    for mode in [ConversationMode::Chat, ConversationMode::Code] {
        let claude =
            resolve_target(Some(&Routes), "shared-model", Some("claude-channel"), mode).unwrap();
        let kimi =
            resolve_target(Some(&Routes), "shared-model", Some("kimi-messages"), mode).unwrap();
        assert_eq!(claude.policy.target.provider_id, "openrouter");
        assert_eq!(claude.policy.target.canonical_model, "claude-opus-5");
        assert_eq!(kimi.policy.target.provider_id, "kimi");
        assert_eq!(kimi.policy.target.canonical_model, "kimi-k2.5");
        assert_eq!(claude.policy.target.mode, mode);
        assert_eq!(kimi.policy.target.mode, mode);
        assert_eq!(
            claude.policy.bundle_id,
            if mode == ConversationMode::Chat {
                "claude-chat"
            } else {
                "claude-code"
            }
        );
        assert_eq!(
            kimi.policy.bundle_id,
            if mode == ConversationMode::Chat {
                "generic-chat-lean-v1"
            } else {
                "generic-code-lean-v1"
            }
        );
        let rendered = state(&kimi).render(&kimi);
        let body = rendered.system.unwrap();
        let body = body.string_at("").unwrap();
        assert!(body.contains("CUSTOM ROLE"));
        assert!(!body.contains("Claude"));
        assert!(!body.contains("<total_tokens>"));
        assert!(!rendered.system_turns);
    }
}

#[test]
fn policy_renderer_preserves_three_live_claude_child_prefixes() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../scripts/tests/fixtures/agent_child_prefix_319.json"
    ))
    .unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let target = resolve_target(
            None,
            case["model"].as_str().unwrap(),
            None,
            ConversationMode::Code,
        )
        .unwrap();
        let mut state = state(&target);
        state.role = Some("WIRE_CHILD_AGENT".into());
        let context = SubagentPromptContext {
            additional_context: case["contextSections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect(),
            system_suffix: case["systemSuffix"].as_str().map(str::to_owned),
        };
        state.environment = Some(Arc::new(move |_, _| context.clone()));
        let rendered = state.render(&target);
        assert_eq!(
            rendered.system.as_ref().unwrap().string_at(""),
            case["systemBody"].as_str(),
            "{}",
            case["model"]
        );
        assert_eq!(
            rendered.system_turns,
            case["contextPlacement"].as_str() == Some("system_after_task")
        );
        let mut messages = vec![ConversationMessage::user(MessageId::new(), "TASK".into())];
        inject_context(&mut messages, &rendered.context, rendered.system_turns);
        if rendered.system_turns {
            assert_eq!(messages.len(), 2);
            assert!(matches!(
                messages[1],
                ConversationMessage::System {
                    api_system: Some(_),
                    ..
                }
            ));
        } else {
            let ConversationMessage::User { content, .. } = &messages[0] else {
                panic!("user task")
            };
            assert_eq!(content.len(), 4);
            assert!(matches!(&content[3], ContentBlock::Text { text, .. } if text == "TASK"));
        }
    }
}

#[test]
fn fork_reuses_exact_prefix_only_for_the_same_policy_and_keeps_custom_utf16() {
    let claude = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("claude-channel"),
        ConversationMode::Code,
    )
    .unwrap();
    let kimi = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("kimi-messages"),
        ConversationMode::Code,
    )
    .unwrap();
    let mut state = state(&claude);
    let role_units = vec![b'R' as u16, 0xd800, b'X' as u16];
    state.role = Some(
        Utf16JsonProjection::root_string(String::from_utf16_lossy(&role_units), role_units.clone())
            .unwrap(),
    );
    state.fork_prefix = Some("PARENT CLAUDE PREFIX".into());
    state.fork_policy = Some(claude.policy.metadata());
    let same = state.render(&claude);
    assert!(same.reuses_fork_prefix);
    assert_eq!(
        same.system.unwrap().string_at(""),
        Some("PARENT CLAUDE PREFIX")
    );
    let changed = state.render(&kimi);
    assert!(!changed.reuses_fork_prefix);
    let body = changed.system.unwrap();
    assert!(!body.string_at("").unwrap().contains("PARENT CLAUDE PREFIX"));
    assert!(body
        .string_units("")
        .unwrap()
        .windows(role_units.len())
        .any(|units| units == role_units));
}

#[test]
fn changed_target_fork_preserves_custom_complete_override_and_append_sources() {
    let claude = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("claude-channel"),
        ConversationMode::Code,
    )
    .unwrap();
    let kimi = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("kimi-messages"),
        ConversationMode::Code,
    )
    .unwrap();
    let mut state = state(&claude);
    state.fork_prefix = Some("EXACT PARENT PREFIX".into());
    state.fork_policy = Some(claude.policy.metadata());
    let custom_units = vec![b'U' as u16, 0xd800, b'R' as u16];
    state.custom_system_prompt = Some(
        Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&custom_units),
            custom_units.clone(),
        )
        .unwrap(),
    );
    state.append_system_prompt = Some("APPENDED USER INSTRUCTIONS".into());
    state.environment = Some(Arc::new(|_, _| SubagentPromptContext {
        additional_context: vec!["CURRENT ENVIRONMENT".into()],
        system_suffix: Some("NATIVE BUDGET".into()),
    }));
    let same = state.render(&claude);
    assert_eq!(
        same.system.unwrap().string_at(""),
        Some("EXACT PARENT PREFIX")
    );
    let changed = state.render(&kimi);
    let mut expected = custom_units;
    expected.extend("\n\nAPPENDED USER INSTRUCTIONS".encode_utf16());
    assert_eq!(changed.system.unwrap().string_units(""), Some(expected));
    assert_eq!(changed.context, ["CURRENT ENVIRONMENT"]);
}

#[test]
fn legacy_fork_without_policy_metadata_rebuilds_instead_of_reusing_a_snapshot() {
    let target = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("kimi-messages"),
        ConversationMode::Code,
    )
    .unwrap();
    let mut state = state(&target);
    state.fork_prefix = Some("LEGACY CLAUDE PREFIX".into());
    let rendered = state.render(&target);
    assert!(!rendered.reuses_fork_prefix);
    let body = rendered.system.unwrap();
    assert!(!body.string_at("").unwrap().contains("LEGACY CLAUDE PREFIX"));
    assert!(body.string_at("").unwrap().contains("CUSTOM ROLE"));
}

#[test]
fn changed_route_projects_fresh_context_without_mutating_history_or_instructions() {
    let claude = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("claude-channel"),
        ConversationMode::Code,
    )
    .unwrap();
    let kimi = resolve_target(
        Some(&Routes),
        "shared-model",
        Some("kimi-messages"),
        ConversationMode::Code,
    )
    .unwrap();
    let mut state = state(&claude);
    state.environment = Some(Arc::new(|target, _| SubagentPromptContext {
        additional_context: vec![format!(
            "# Environment\n - Model: {}",
            target.policy.target.canonical_model
        )],
        system_suffix: None,
    }));
    let original = state.render(&claude);
    let mut history = vec![ConversationMessage::user(
        MessageId::new(),
        "PRESERVE TASK".into(),
    )];
    state.seeded_context = inject_context(&mut history, &original.context, original.system_turns);
    history.push(ConversationMessage::user_meta(
        MessageId::new(),
        "PROJECT INSTRUCTIONS".into(),
    ));
    let unchanged = history.clone();
    let rendered = state.render(&kimi);
    let mut snapshot = history.clone();
    state.project_context(&mut snapshot, &rendered);
    assert_eq!(history, unchanged);
    let text = snapshot
        .iter()
        .map(ConversationMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("kimi-k2.5"));
    assert!(!text.contains("claude-opus-5"));
    assert!(text.contains("PRESERVE TASK"));
    assert!(text.contains("PROJECT INSTRUCTIONS"));
}
