//! Versioned prompt selection from a trusted request route.

use super::model_capabilities::{is_known_model_id, normalize_model_id};
use super::session_context::ConversationMode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod registry;
pub use registry::{PromptModelSelector, PromptRegistry, PromptRegistryError, PromptRule};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptTarget {
    pub mode: ConversationMode,
    pub provider_id: String,
    pub request_model: String,
    pub canonical_model: String,
    pub model_family: Option<String>,
    pub profile_name: Option<String>,
}

impl PromptTarget {
    #[must_use]
    pub fn for_model(mode: ConversationMode, model: &str) -> Self {
        Self::from_route(mode, "", model, None, None, None)
    }

    #[must_use]
    pub fn unresolved(model: &str, profile: Option<&str>, mode: ConversationMode) -> Self {
        Self::from_route(mode, "", model, None, None, profile)
    }

    #[must_use]
    pub fn from_route(
        mode: ConversationMode,
        provider_id: &str,
        request_model: &str,
        underlying_model: Option<&str>,
        model_family: Option<&str>,
        profile_name: Option<&str>,
    ) -> Self {
        let identity = underlying_model.unwrap_or(request_model);
        let candidate = identity
            .strip_prefix("openrouter/anthropic/")
            .or_else(|| identity.strip_prefix("anthropic/"))
            .unwrap_or(identity);
        let wrapper_known = !candidate.contains('/');
        let normalized = normalize_model_id(candidate);
        let normalized = if normalized.starts_with("claude-") {
            normalize_model_id(&normalized.replace('.', "-"))
        } else {
            normalized
        };
        let claude = wrapper_known
            && (is_known_model_id(&normalized)
                || matches!(
                    normalized.as_str(),
                    "claude-3-opus" | "claude-3-sonnet" | "claude-3-haiku"
                ));
        Self {
            mode,
            provider_id: provider_id.to_owned(),
            request_model: request_model.to_owned(),
            canonical_model: if claude {
                normalized
            } else {
                identity.to_owned()
            },
            model_family: if claude {
                Some("claude".into())
            } else {
                model_family.map(str::to_owned)
            },
            profile_name: profile_name.map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct PromptTemplateOverride {
    pub key: &'static str,
    pub template: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SystemPromptBinding {
    ClaudeCode,
    ClaudeChat,
    Template(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ToolPromptBinding {
    ClaudeNative,
    Lean {
        overrides: &'static [PromptTemplateOverride],
    },
}

impl ToolPromptBinding {
    #[must_use]
    pub const fn is_native(self) -> bool {
        matches!(self, Self::ClaudeNative)
    }

    #[must_use]
    pub fn template_for(self, tool_name: &str) -> Option<&'static str> {
        match self {
            Self::ClaudeNative => None,
            Self::Lean { overrides } => overrides
                .iter()
                .find(|row| row.key == tool_name)
                .map(|row| row.template),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum MemoryPromptBinding {
    ClaudeNative,
    Template(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ReminderPromptBinding {
    ClaudeNative,
    Portable {
        base: &'static [PromptTemplateOverride],
        overrides: &'static [PromptTemplateOverride],
    },
}

impl ReminderPromptBinding {
    #[must_use]
    pub const fn is_native(self) -> bool {
        matches!(self, Self::ClaudeNative)
    }

    #[must_use]
    pub fn template_for(self, kind: &str) -> Option<&'static str> {
        match self {
            Self::ClaudeNative => None,
            Self::Portable { base, overrides } => overrides
                .iter()
                .chain(base)
                .find(|row| row.key == kind)
                .map(|row| row.template),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct PromptBundle {
    pub id: &'static str,
    pub version: &'static str,
    pub system: SystemPromptBinding,
    pub tools: ToolPromptBinding,
    pub memory: MemoryPromptBinding,
    pub reminders: ReminderPromptBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ResolvedPromptPolicy {
    pub target: PromptTarget,
    pub bundle_id: &'static str,
    pub bundle_version: &'static str,
    pub matched_rule: &'static str,
    pub system: SystemPromptBinding,
    pub tools: ToolPromptBinding,
    pub memory: MemoryPromptBinding,
    pub reminders: ReminderPromptBinding,
}

impl ResolvedPromptPolicy {
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("prompt policy has only serializable facts");
        format!("{:x}", Sha256::digest(bytes))
    }

    #[must_use]
    pub fn metadata(&self) -> PromptPolicyMetadata {
        PromptPolicyMetadata {
            schema_version: 1,
            target: self.target.clone(),
            bundle_id: self.bundle_id.to_owned(),
            bundle_version: self.bundle_version.to_owned(),
            matched_rule: self.matched_rule.to_owned(),
            fingerprint: self.fingerprint(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptPolicyMetadata {
    pub schema_version: u32,
    pub target: PromptTarget,
    pub bundle_id: String,
    pub bundle_version: String,
    pub matched_rule: String,
    pub fingerprint: String,
}

#[must_use]
pub fn resolve_prompt_policy(target: PromptTarget) -> ResolvedPromptPolicy {
    registry::resolve_builtin(target)
}

#[cfg(test)]
mod tests;
