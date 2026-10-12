//! Host contracts for Claude Code's per-session output-style and language rows.

use crate::types::utf16_json::Utf16JsonProjection;
use crate::types::{ConversationMessage, MessageId};
use async_trait::async_trait;

/// The real query-agent identity used by Native's `fPr` settings-attachment gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSettingsAgentContext {
    /// The interactive or ordinary headless session.
    Main,
    /// An in-process teammate in a coordinated team.
    Teammate,
    /// A subagent, with Native's explicit `isMainSession` fact.
    Subagent { is_main_session: bool },
}

impl NativeSettingsAgentContext {
    /// Whether Native admits this identity once its option gates also pass.
    #[must_use]
    pub fn is_admitted(self) -> bool {
        match self {
            Self::Main | Self::Teammate => true,
            Self::Subagent { is_main_session } => is_main_session,
        }
    }
}

/// The `N9e` option facts paired with a query-agent identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeSettingsPromptContext {
    /// The Native query context; this is not inferred from a model or task name.
    pub agent_context: NativeSettingsAgentContext,
    /// Native `options.bareFork` for this query.
    pub bare_fork: Option<bool>,
    /// Native `options.isolatedContext` for this query.
    pub isolated_context: Option<bool>,
}

impl NativeSettingsPromptContext {
    /// Whether the source-level `fPr` and `N9e` gates admit this query.
    #[must_use]
    pub fn is_admitted(self) -> bool {
        self.agent_context.is_admitted()
            && !self.bare_fork.unwrap_or(false)
            && !self.isolated_context.unwrap_or(false)
    }
}

/// A typed model-visible settings row, retaining both source JSON and rendered text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSettingsPromptAttachment {
    /// Stable model-history row paired with `payload`.
    pub message: ConversationMessage,
    /// Exact typed Native payload, including UTF-16 source units.
    pub payload: Utf16JsonProjection,
    /// Exact rendered reminder projection written beside the payload.
    pub rendered: Utf16JsonProjection,
}

/// Builds settings deltas against a query's own previous typed rows.
#[async_trait]
pub trait NativeSettingsPromptAttachmentProvider: Send + Sync {
    /// Produce output-style then language rows from the current live settings.
    async fn native_settings_prompt_attachments(
        &self,
        context: NativeSettingsPromptContext,
        prior_payloads: &[(MessageId, Utf16JsonProjection)],
    ) -> Vec<NativeSettingsPromptAttachment>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_and_option_gates_are_applied_exactly() {
        for identity in [
            NativeSettingsAgentContext::Main,
            NativeSettingsAgentContext::Teammate,
            NativeSettingsAgentContext::Subagent {
                is_main_session: true,
            },
        ] {
            assert!(NativeSettingsPromptContext {
                agent_context: identity,
                bare_fork: None,
                isolated_context: None,
            }
            .is_admitted());
        }

        for identity in [
            NativeSettingsAgentContext::Subagent {
                is_main_session: false,
            },
            NativeSettingsAgentContext::Subagent {
                is_main_session: true,
            },
        ] {
            assert!(!NativeSettingsPromptContext {
                agent_context: identity,
                bare_fork: Some(true),
                isolated_context: None,
            }
            .is_admitted());
            assert!(!NativeSettingsPromptContext {
                agent_context: identity,
                bare_fork: None,
                isolated_context: Some(true),
            }
            .is_admitted());
        }

        assert!(!NativeSettingsPromptContext {
            agent_context: NativeSettingsAgentContext::Subagent {
                is_main_session: false,
            },
            bare_fork: None,
            isolated_context: None,
        }
        .is_admitted());
    }
}
