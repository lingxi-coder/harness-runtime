//! Host recovery policy. Provider codecs never decide to repeat side effects.
pub(crate) const INCOMPLETE_TOOL_NUDGE: &str = "A tool call was interrupted before its arguments were complete. Keep completed tool results. Generate a complete call only for the unfinished work, using smaller chunks if necessary. Do not repeat tools that already completed.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputRecoveryAction {
    ContinueText,
    RegenerateIncompleteTool,
    KeepToolResultsAndRepair,
    Stop,
}

/// Terminal facts only: a transport failure or parser diagnosis alone never
/// establishes that generation reached a token limit.
pub(crate) struct CompletionFacts<'a> {
    pub stop_reason: Option<&'a str>,
    pub malformed_tool: bool,
    pub accepted_tools: bool,
    pub actions_used: u32,
}
pub(crate) fn decide_output_recovery(facts: CompletionFacts<'_>) -> OutputRecoveryAction {
    if facts.actions_used >= 3 {
        return OutputRecoveryAction::Stop;
    }
    if facts.malformed_tool
        && matches!(
            facts.stop_reason,
            Some("max_tokens" | "tool_use" | "end_turn" | "stop_sequence")
        )
    {
        return if facts.accepted_tools {
            OutputRecoveryAction::KeepToolResultsAndRepair
        } else {
            OutputRecoveryAction::RegenerateIncompleteTool
        };
    }
    if facts.stop_reason == Some("max_tokens") {
        OutputRecoveryAction::ContinueText
    } else {
        OutputRecoveryAction::Stop
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_reason_and_side_effects_choose_recovery() {
        for reason in [None, Some("aborted"), Some("insufficient_system_resource")] {
            assert_eq!(
                decide_output_recovery(CompletionFacts {
                    stop_reason: reason,
                    malformed_tool: true,
                    accepted_tools: false,
                    actions_used: 0
                }),
                OutputRecoveryAction::Stop
            );
        }
        assert_eq!(
            decide_output_recovery(CompletionFacts {
                stop_reason: Some("max_tokens"),
                malformed_tool: true,
                accepted_tools: false,
                actions_used: 0
            }),
            OutputRecoveryAction::RegenerateIncompleteTool
        );
        assert_eq!(
            decide_output_recovery(CompletionFacts {
                stop_reason: Some("max_tokens"),
                malformed_tool: true,
                accepted_tools: true,
                actions_used: 0
            }),
            OutputRecoveryAction::KeepToolResultsAndRepair
        );
        assert_eq!(
            decide_output_recovery(CompletionFacts {
                stop_reason: Some("max_tokens"),
                malformed_tool: false,
                accepted_tools: false,
                actions_used: 3
            }),
            OutputRecoveryAction::Stop
        );
    }
}

#[derive(Clone)]
pub(crate) struct FrozenGeneration {
    pub model: String,
    pub profile: Option<String>,
    pub system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    pub skip_global_cache: bool,
}

impl std::fmt::Debug for FrozenGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrozenGeneration")
            .field("model", &self.model)
            .field("profile", &self.profile)
            .field("message_count", &self.messages.len())
            .field("tool_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}
