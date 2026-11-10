use super::*;

const TEST_BUNDLES: &[PromptBundle] = &[PromptBundle {
    id: "test",
    version: "1",
    system: SystemPromptBinding::Template("Full replacement system template"),
    tools: ToolPromptBinding::Lean {
        overrides: &[PromptTemplateOverride {
            key: "Read",
            template: "Read override",
        }],
    },
    memory: MemoryPromptBinding::Template("Memory directory: {memory_path}"),
    reminders: ReminderPromptBinding::Portable {
        base: &[PromptTemplateOverride {
            key: "progress",
            template: "base reminder",
        }],
        overrides: &[PromptTemplateOverride {
            key: "progress",
            template: "replacement reminder",
        }],
    },
}];

const CODE_FALLBACK: PromptRule = PromptRule {
    id: "mode-default",
    mode: ConversationMode::Code,
    provider_id: None,
    model: PromptModelSelector::Any,
    bundle_id: "test",
};
const CHAT_FALLBACK: PromptRule = PromptRule {
    id: "chat-default",
    mode: ConversationMode::Chat,
    ..CODE_FALLBACK
};

#[test]
fn six_level_rules_are_order_independent_and_mode_scoped() {
    const RULES: &[PromptRule] = &[
        CODE_FALLBACK,
        CHAT_FALLBACK,
        PromptRule {
            id: "provider-default",
            provider_id: Some("provider-a"),
            ..CODE_FALLBACK
        },
        PromptRule {
            id: "global-family",
            model: PromptModelSelector::Family("family-a"),
            ..CODE_FALLBACK
        },
        PromptRule {
            id: "provider-family",
            provider_id: Some("provider-a"),
            model: PromptModelSelector::Family("family-a"),
            ..CODE_FALLBACK
        },
        PromptRule {
            id: "global-model",
            model: PromptModelSelector::Exact("model-a"),
            ..CODE_FALLBACK
        },
        PromptRule {
            id: "provider-model",
            provider_id: Some("provider-a"),
            model: PromptModelSelector::Exact("model-a"),
            ..CODE_FALLBACK
        },
    ];
    let registry = PromptRegistry::new(TEST_BUNDLES, RULES).unwrap();
    for (provider, model, family, expected) in [
        ("provider-a", "model-a", "family-a", "provider-model"),
        ("provider-b", "model-a", "family-a", "global-model"),
        ("provider-a", "model-b", "family-a", "provider-family"),
        ("provider-b", "model-b", "family-a", "global-family"),
        ("provider-a", "model-b", "family-b", "provider-default"),
        ("provider-b", "model-b", "family-b", "mode-default"),
    ] {
        let target = PromptTarget::from_route(
            ConversationMode::Code,
            provider,
            model,
            None,
            Some(family),
            Some("connection"),
        );
        let policy = registry.resolve(target.clone()).unwrap();
        assert_eq!(policy.target, target);
        assert_eq!(policy.matched_rule, expected);
        let chat = registry
            .resolve(PromptTarget {
                mode: ConversationMode::Chat,
                ..target
            })
            .unwrap();
        assert_eq!(chat.matched_rule, "chat-default");
    }
}

#[test]
fn bundles_replace_every_component_without_assembler_changes() {
    let registry = PromptRegistry::new(TEST_BUNDLES, &[CODE_FALLBACK, CHAT_FALLBACK]).unwrap();
    let policy = registry
        .resolve(PromptTarget::for_model(ConversationMode::Code, "model-a"))
        .unwrap();
    assert_eq!(
        policy.system,
        SystemPromptBinding::Template("Full replacement system template")
    );
    assert_eq!(policy.tools.template_for("Read"), Some("Read override"));
    assert_eq!(policy.tools.template_for("Write"), None);
    assert_eq!(
        policy.memory,
        MemoryPromptBinding::Template("Memory directory: {memory_path}")
    );
    assert_eq!(
        policy.reminders.template_for("progress"),
        Some("replacement reminder")
    );
    assert_eq!(policy.reminders.template_for("native-only"), None);
}

#[test]
fn registry_rejects_duplicate_or_unresolvable_configuration() {
    const DUPLICATE_BUNDLES: &[PromptBundle] = &[TEST_BUNDLES[0], TEST_BUNDLES[0]];
    assert_eq!(
        PromptRegistry::new(DUPLICATE_BUNDLES, &[]).unwrap_err(),
        PromptRegistryError::DuplicateBundleId("test")
    );
    assert!(matches!(
        PromptRegistry::new(TEST_BUNDLES, &[CODE_FALLBACK, CODE_FALLBACK]),
        Err(PromptRegistryError::DuplicateRuleId("mode-default"))
    ));
    assert!(matches!(
        PromptRegistry::new(
            TEST_BUNDLES,
            &[
                CODE_FALLBACK,
                PromptRule {
                    id: "duplicate-selector",
                    ..CODE_FALLBACK
                }
            ]
        ),
        Err(PromptRegistryError::DuplicateRule { .. })
    ));
    assert!(matches!(
        PromptRegistry::new(
            TEST_BUNDLES,
            &[PromptRule {
                bundle_id: "missing",
                ..CODE_FALLBACK
            }]
        ),
        Err(PromptRegistryError::UnknownBundle { .. })
    ));
    assert!(matches!(
        PromptRegistry::new(TEST_BUNDLES, &[CODE_FALLBACK]),
        Err(PromptRegistryError::MissingModeFallback(
            ConversationMode::Chat
        ))
    ));
}

#[test]
fn default_matrix_keeps_claude_across_supported_transports() {
    for mode in [ConversationMode::Chat, ConversationMode::Code] {
        for (provider, model) in [
            ("anthropic", "claude-sonnet-4-6"),
            ("openrouter", "anthropic/claude-opus-4.6"),
            ("openrouter", "openrouter/anthropic/claude-opus-5-5"),
            ("bedrock-claude", "us.anthropic.claude-opus-4-6-v1"),
            ("vertex-claude", "claude-sonnet-4-5@20250929"),
        ] {
            let target =
                PromptTarget::from_route(mode, provider, model, None, None, Some("profile"));
            let policy = resolve_prompt_policy(target);
            assert!(policy.tools.is_native(), "{provider}: {model}");
            assert!(policy.reminders.is_native());
            assert_eq!(policy.bundle_version, "2.1.319-lingxi.1");
            assert_eq!(
                policy.system,
                if mode == ConversationMode::Chat {
                    SystemPromptBinding::ClaudeChat
                } else {
                    SystemPromptBinding::ClaudeCode
                }
            );
        }
        for (provider, model, family) in [
            ("openai", "gpt-6", Some("gpt")),
            ("google", "gemini-3.5", Some("gemini")),
            ("kimi", "kimi-k3", Some("kimi-k3")),
            ("custom", "vendor/claude-opus-5-5", None),
            ("custom", "vendor/qwen3", None),
            ("custom", "claude-future-unknown", None),
        ] {
            let target = PromptTarget::from_route(mode, provider, model, None, family, None);
            assert_eq!(target.canonical_model, model);
            let policy = resolve_prompt_policy(target);
            assert!(!policy.tools.is_native(), "{provider}: {model}");
            assert!(!policy.reminders.is_native());
            assert_eq!(
                policy.bundle_id,
                if mode == ConversationMode::Chat {
                    "generic-chat-lean-v1"
                } else {
                    "generic-code-lean-v1"
                }
            );
        }
    }
}

#[test]
fn every_native_catalog_provider_identity_resolves_to_the_same_claude_bundle() {
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/model_capabilities_2_1_319.json"
    ))
    .unwrap();
    for row in oracle["models"].as_array().unwrap() {
        let canonical = row["id"].as_str().unwrap();
        for (provider, model) in row["provider_ids"].as_object().unwrap() {
            let Some(model) = model.as_str() else {
                continue;
            };
            for mode in [ConversationMode::Chat, ConversationMode::Code] {
                let target = PromptTarget::from_route(mode, provider, model, None, None, None);
                assert_eq!(target.canonical_model, canonical, "{provider}: {model}");
                let policy = resolve_prompt_policy(target);
                assert!(policy.tools.is_native(), "{provider}: {model}");
                assert!(policy.reminders.is_native());
            }
        }
    }
}

#[test]
fn unknown_namespaces_and_request_identity_are_preserved() {
    for model in [
        "vendor/claude-opus-5-5",
        "vendor/qwen/model",
        "Vendor/Model",
        "x/anthropic/claude-opus-5",
    ] {
        let target =
            PromptTarget::unresolved(model, Some("configured-profile"), ConversationMode::Code);
        assert_eq!(target.request_model, model);
        assert_eq!(target.canonical_model, model);
        assert_eq!(target.provider_id, "");
        assert_eq!(target.profile_name.as_deref(), Some("configured-profile"));
        assert_eq!(target.model_family, None);
    }
    let target = PromptTarget::from_route(
        ConversationMode::Chat,
        "foundry-claude",
        "customer-deployment",
        Some("claude-opus-5-5"),
        None,
        Some("foundry"),
    );
    assert_eq!(target.request_model, "customer-deployment");
    assert_eq!(target.canonical_model, "claude-opus-5-5");
    assert_eq!(target.model_family.as_deref(), Some("claude"));

    let dotted =
        PromptTarget::for_model(ConversationMode::Code, "anthropic/claude-opus-4.6-20260204");
    assert_eq!(dotted.canonical_model, "claude-opus-4-6");
}

#[test]
fn metadata_is_default_compatible_and_contains_no_template_body() {
    let policy = resolve_prompt_policy(PromptTarget::for_model(ConversationMode::Code, "gpt-6"));
    let metadata = policy.metadata();
    let json = serde_json::to_string(&metadata).unwrap();
    assert!(!json.contains("Tools and actions"));
    assert_eq!(
        serde_json::from_str::<PromptPolicyMetadata>(&json).unwrap(),
        metadata
    );
    assert_eq!(
        serde_json::from_str::<PromptPolicyMetadata>("{}").unwrap(),
        PromptPolicyMetadata::default()
    );
    assert_eq!(metadata.fingerprint.len(), 64);
    assert_eq!(metadata.fingerprint, policy.fingerprint());
}

#[test]
fn fingerprint_fences_mode_provider_profile_model_version_and_template_content() {
    let policy = resolve_prompt_policy(PromptTarget::for_model(ConversationMode::Code, "gpt-6"));
    let original = policy.fingerprint();
    let mut changed = policy.clone();
    changed.target.mode = ConversationMode::Chat;
    assert_ne!(original, changed.fingerprint());
    changed = policy.clone();
    changed.target.provider_id = "another-provider".into();
    assert_ne!(original, changed.fingerprint());
    changed = policy.clone();
    changed.target.profile_name = Some("another-profile".into());
    assert_ne!(original, changed.fingerprint());
    changed = policy.clone();
    changed.target.request_model = "another-model".into();
    assert_ne!(original, changed.fingerprint());
    changed = policy.clone();
    changed.bundle_version = "3";
    assert_ne!(original, changed.fingerprint());
    changed = policy.clone();
    changed.system = SystemPromptBinding::Template("Changed template");
    assert_ne!(original, changed.fingerprint());
}
