use super::{
    ConversationMode, MemoryPromptBinding, PromptBundle, PromptTarget, PromptTemplateOverride,
    ReminderPromptBinding, ResolvedPromptPolicy, SystemPromptBinding, ToolPromptBinding,
};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptModelSelector {
    Exact(&'static str),
    Family(&'static str),
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptRule {
    pub id: &'static str,
    pub mode: ConversationMode,
    pub provider_id: Option<&'static str>,
    pub model: PromptModelSelector,
    pub bundle_id: &'static str,
}

impl PromptRule {
    fn priority(self) -> u8 {
        match (self.provider_id.is_some(), self.model) {
            (true, PromptModelSelector::Exact(_)) => 0,
            (false, PromptModelSelector::Exact(_)) => 1,
            (true, PromptModelSelector::Family(_)) => 2,
            (false, PromptModelSelector::Family(_)) => 3,
            (true, PromptModelSelector::Any) => 4,
            (false, PromptModelSelector::Any) => 5,
        }
    }

    fn matches(self, target: &PromptTarget) -> bool {
        self.mode == target.mode
            && self
                .provider_id
                .is_none_or(|provider| provider == target.provider_id)
            && match self.model {
                PromptModelSelector::Exact(model) => model == target.canonical_model,
                PromptModelSelector::Family(family) => {
                    target.model_family.as_deref() == Some(family)
                }
                PromptModelSelector::Any => true,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptRegistryError {
    #[error("duplicate prompt bundle id: {0}")]
    DuplicateBundleId(&'static str),
    #[error("duplicate prompt rule id: {0}")]
    DuplicateRuleId(&'static str),
    #[error("prompt rules {first} and {second} have identical selectors")]
    DuplicateRule {
        first: &'static str,
        second: &'static str,
    },
    #[error("prompt rule {rule} refers to unavailable bundle {bundle}")]
    UnknownBundle {
        rule: &'static str,
        bundle: &'static str,
    },
    #[error("missing default prompt rule for mode {0:?}")]
    MissingModeFallback(ConversationMode),
    #[error("no matching prompt rule for mode {0:?}")]
    NoMatchingRule(ConversationMode),
    #[error("duplicate template override {key} in bundle {bundle}")]
    DuplicateTemplateOverride {
        bundle: &'static str,
        key: &'static str,
    },
}

#[derive(Debug)]
pub struct PromptRegistry {
    bundles: &'static [PromptBundle],
    rules: &'static [PromptRule],
}

impl PromptRegistry {
    pub fn new(
        bundles: &'static [PromptBundle],
        rules: &'static [PromptRule],
    ) -> Result<Self, PromptRegistryError> {
        for (index, bundle) in bundles.iter().enumerate() {
            if bundles[..index].iter().any(|other| other.id == bundle.id) {
                return Err(PromptRegistryError::DuplicateBundleId(bundle.id));
            }
            let mut sets = Vec::new();
            if let ToolPromptBinding::Lean { overrides } = bundle.tools {
                sets.push(overrides);
            }
            if let ReminderPromptBinding::Portable { base, overrides } = bundle.reminders {
                sets.extend([base, overrides]);
            }
            for templates in sets {
                for (position, row) in templates.iter().enumerate() {
                    if templates[..position]
                        .iter()
                        .any(|other| other.key == row.key)
                    {
                        return Err(PromptRegistryError::DuplicateTemplateOverride {
                            bundle: bundle.id,
                            key: row.key,
                        });
                    }
                }
            }
        }
        for (index, rule) in rules.iter().enumerate() {
            if rules[..index].iter().any(|other| other.id == rule.id) {
                return Err(PromptRegistryError::DuplicateRuleId(rule.id));
            }
            if let Some(other) = rules[..index].iter().find(|other| {
                other.mode == rule.mode
                    && other.provider_id == rule.provider_id
                    && other.model == rule.model
            }) {
                return Err(PromptRegistryError::DuplicateRule {
                    first: other.id,
                    second: rule.id,
                });
            }
            if !bundles.iter().any(|bundle| bundle.id == rule.bundle_id) {
                return Err(PromptRegistryError::UnknownBundle {
                    rule: rule.id,
                    bundle: rule.bundle_id,
                });
            }
        }
        for mode in [ConversationMode::Chat, ConversationMode::Code] {
            if !rules.iter().any(|rule| {
                rule.mode == mode
                    && rule.provider_id.is_none()
                    && rule.model == PromptModelSelector::Any
            }) {
                return Err(PromptRegistryError::MissingModeFallback(mode));
            }
        }
        Ok(Self { bundles, rules })
    }

    pub fn resolve(
        &self,
        target: PromptTarget,
    ) -> Result<ResolvedPromptPolicy, PromptRegistryError> {
        let rule = self
            .rules
            .iter()
            .filter(|rule| rule.matches(&target))
            .min_by_key(|rule| rule.priority())
            .ok_or(PromptRegistryError::NoMatchingRule(target.mode))?;
        let bundle = self
            .bundles
            .iter()
            .find(|bundle| bundle.id == rule.bundle_id)
            .expect("registry validation guarantees every rule's bundle");
        Ok(ResolvedPromptPolicy {
            target,
            bundle_id: bundle.id,
            bundle_version: bundle.version,
            matched_rule: rule.id,
            system: bundle.system,
            tools: bundle.tools,
            memory: bundle.memory,
            reminders: bundle.reminders,
        })
    }
}

const PORTABLE_REMINDERS: &[PromptTemplateOverride] = &[
    PromptTemplateOverride {
        key: "subagent_consent",
        template: include_str!("../prompt_templates/subagent-consent.md"),
    },
    PromptTemplateOverride {
        key: "subagent_notes",
        template: include_str!("../prompt_templates/subagent-notes.md"),
    },
];

const BUNDLES: &[PromptBundle] = &[
    PromptBundle {
        id: "claude-code",
        version: "2.1.319",
        system: SystemPromptBinding::ClaudeCode,
        tools: ToolPromptBinding::ClaudeNative,
        memory: MemoryPromptBinding::ClaudeNative,
        reminders: ReminderPromptBinding::ClaudeNative,
    },
    PromptBundle {
        id: "claude-chat",
        version: "2.1.319",
        system: SystemPromptBinding::ClaudeChat,
        tools: ToolPromptBinding::ClaudeNative,
        memory: MemoryPromptBinding::ClaudeNative,
        reminders: ReminderPromptBinding::ClaudeNative,
    },
    PromptBundle {
        id: "generic-code-lean-v1",
        version: "1",
        system: SystemPromptBinding::Template(include_str!(
            "../prompt_templates/generic-code-lean.md"
        )),
        tools: ToolPromptBinding::Lean { overrides: &[] },
        memory: MemoryPromptBinding::Template(include_str!(
            "../prompt_templates/generic-memory.md"
        )),
        reminders: ReminderPromptBinding::Portable {
            base: PORTABLE_REMINDERS,
            overrides: &[],
        },
    },
    PromptBundle {
        id: "generic-chat-lean-v1",
        version: "1",
        system: SystemPromptBinding::Template(include_str!(
            "../prompt_templates/generic-chat-lean.md"
        )),
        tools: ToolPromptBinding::Lean { overrides: &[] },
        memory: MemoryPromptBinding::Template(include_str!(
            "../prompt_templates/generic-memory.md"
        )),
        reminders: ReminderPromptBinding::Portable {
            base: PORTABLE_REMINDERS,
            overrides: &[],
        },
    },
];

const RULES: &[PromptRule] = &[
    PromptRule {
        id: "claude-code-family",
        mode: ConversationMode::Code,
        provider_id: None,
        model: PromptModelSelector::Family("claude"),
        bundle_id: "claude-code",
    },
    PromptRule {
        id: "claude-chat-family",
        mode: ConversationMode::Chat,
        provider_id: None,
        model: PromptModelSelector::Family("claude"),
        bundle_id: "claude-chat",
    },
    PromptRule {
        id: "code-default",
        mode: ConversationMode::Code,
        provider_id: None,
        model: PromptModelSelector::Any,
        bundle_id: "generic-code-lean-v1",
    },
    PromptRule {
        id: "chat-default",
        mode: ConversationMode::Chat,
        provider_id: None,
        model: PromptModelSelector::Any,
        bundle_id: "generic-chat-lean-v1",
    },
];

static BUILTIN_REGISTRY: LazyLock<PromptRegistry> = LazyLock::new(|| {
    PromptRegistry::new(BUNDLES, RULES).expect("built-in prompt registry must be valid")
});

pub(super) fn resolve_builtin(target: PromptTarget) -> ResolvedPromptPolicy {
    BUILTIN_REGISTRY
        .resolve(target)
        .expect("every mode has a built-in fallback")
}
