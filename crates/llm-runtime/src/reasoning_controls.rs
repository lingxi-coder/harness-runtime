//! Host user-intent adapter for the SDK's reasoning controls.
use crate::{LlmError, LlmRequest};
pub use lingxi_llm_client::reasoning::{
    reasoning_control_spec, ReasoningControlSpec, ReasoningSelection, ReasoningTarget,
    TokenBudgetRange,
};

pub fn apply_reasoning_selection(
    request: &mut LlmRequest,
    target: ReasoningTarget<'_>,
    selection: ReasoningSelection,
) -> Result<(), LlmError> {
    lingxi_llm_client::reasoning::apply_reasoning_selection(&mut request.input, target, selection)
        .map_err(|message| LlmError::InvalidRequest { message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtocolFamily;
    use lingxi_llm_client::protocol::{ReasoningEffort, ThinkingConfig, ThinkingMode};

    #[test]
    fn invalid_selection_preserves_host_intent() {
        let mut request = LlmRequest::new("unknown-model");
        let thinking = ThinkingConfig {
            mode: Some(ThinkingMode::Adaptive),
            effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        request.input.thinking = Some(thinking.clone());
        let target = ReasoningTarget {
            profile_name: None,
            protocol: &ProtocolFamily::OpenAiResponses,
            base_url: "https://api.openai.com/v1",
            model: "unknown-model",
        };
        assert!(
            apply_reasoning_selection(&mut request, target, ReasoningSelection::Enabled).is_err()
        );
        assert_eq!(request.input.thinking, Some(thinking));
        apply_reasoning_selection(&mut request, target, ReasoningSelection::Automatic).unwrap();
        assert!(request.input.thinking.is_none());
    }

    #[test]
    fn apply_selection_sets_canonical_thinking_controls() {
        let target = ReasoningTarget {
            profile_name: None,
            protocol: &ProtocolFamily::OpenAiResponses,
            base_url: "https://api.openai.com/v1",
            model: "gpt-5",
        };
        let mut request = LlmRequest::new("gpt-5");
        apply_reasoning_selection(
            &mut request,
            target,
            ReasoningSelection::Level("high".to_string()),
        )
        .unwrap();
        assert_eq!(
            request.input.thinking,
            Some(ThinkingConfig {
                effort: Some(ReasoningEffort::High),
                ..Default::default()
            })
        );
    }
}
