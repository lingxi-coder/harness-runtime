//! Local context policy admission; hard failures require provider-count evidence.
use crate::{LlmError, ResolvedContext};

pub(crate) fn check(
    context: &ResolvedContext,
    estimate: u64,
    exact: Option<u64>,
) -> Result<(), LlmError> {
    if let (Some(count), Some(limit)) = (exact, context.hard_input_limit) {
        if count > limit {
            return Err(LlmError::ContextOverflow {
                token_gap: count - limit,
            });
        }
    }
    let count = exact.unwrap_or(estimate);
    let soft = context
        .effective_input_budget
        .filter(|budget| context.hard_input_limit.is_none_or(|hard| *budget < hard));
    if let Some(budget) = soft.filter(|budget| count > *budget) {
        let source = if exact.is_some() {
            "provider-counted"
        } else {
            "estimated"
        };
        return Err(LlmError::InvalidRequest { message: format!("Application input budget exceeded: {source} {count} tokens, budget {budget}. Compact the conversation, select long context, or increase the custom input budget. The input was not truncated.") });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> ResolvedContext {
        let profile: lingxi_llm_client::protocol::ProviderProfile = serde_json::from_value(serde_json::json!({
            "provider_id":"custom","profile_name":"custom","base_url":"https://example.test","protocol":"open_ai_chat","auth":"none",
            "models":[{"request_model":"m","display_model":"m","billing_model":"m","metadata":{"contextWindowTokens":1000000,"context":{"application":{"mode":"custom","input_tokens":200000}}}}]
        })).unwrap();
        lingxi_llm_client::client::context::resolve_context(
            &profile,
            &profile.models[0],
            &Default::default(),
            None,
            &Default::default(),
        )
        .unwrap()
    }
    #[test]
    fn exact_count_overrides_pessimistic_estimate() {
        assert!(check(&context(), 300000, Some(150000)).is_ok());
    }
    #[test]
    fn soft_limit_is_inclusive_and_has_actionable_local_error() {
        assert!(check(&context(), 200000, None).is_ok());
        let error = check(&context(), 200001, None).unwrap_err();
        assert!(
            matches!(error,LlmError::InvalidRequest { ref message } if message.contains("estimated") && message.contains("not truncated"))
        );
    }
    #[test]
    fn only_verified_count_is_a_hard_overflow() {
        let mut c = context();
        c.effective_input_budget = c.hard_input_limit;
        assert!(check(&c, 2000000, None).is_ok());
        assert_eq!(
            check(&c, 1, Some(1000001)),
            Err(LlmError::ContextOverflow { token_gap: 1 })
        );
    }
}
