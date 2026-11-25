//! Tests for `service.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod service_test;`.

pub use super::*;

// Shared across all service test modules: flags are process-global, so retry
// fixtures must exclude the dispatch-header writers in sibling modules too.
static DISPATCH_FLAG_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn cedar_lattice_write() -> std::sync::RwLockWriteGuard<'static, ()> {
    DISPATCH_FLAG_LOCK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn cedar_lattice_read() -> std::sync::RwLockReadGuard<'static, ()> {
    DISPATCH_FLAG_LOCK
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[path = "service_test/fixtures_test.rs"]
mod tests;

#[cfg(test)]
mod tool_result_reminder_suffix_tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};

    #[test]
    fn only_anthropic_messages_moves_the_typed_reminder_into_its_tool_result() {
        let reminder_id = MessageId::new();
        let target_id = MessageId::new();
        let reminder_text = Utf16JsonProjection::from("<system-reminder>tokens</system-reminder>");
        let make_messages = || {
            vec![
                ConversationMessage::User {
                    id: target_id,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: ToolUseId::from("toolu_target"),
                        content: Utf16JsonProjection::from("tool output"),
                        is_error: None,
                        provider_tool_use_id: None,
                        content_blocks: None,
                    }],
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                    api_message_override: None,
                    interrupted_by_shutdown: None,
                    refused_message_id: None,
                },
                ConversationMessage::user_meta(
                    reminder_id,
                    reminder_text.string_at("").unwrap().to_owned(),
                ),
            ]
        };
        let suffix = crate::convert::AnthropicToolResultReminderSuffix {
            tool_use_id: ToolUseId::from("toolu_target"),
            reminder_message_id: reminder_id,
            text: reminder_text.clone(),
        };

        let mut openai_messages = make_messages();
        let original_len = openai_messages.len();
        apply_anthropic_tool_result_reminder_suffixes(
            lingxi_llm_client::protocol::ProtocolFamily::OpenAiChat,
            &mut openai_messages,
            std::slice::from_ref(&suffix),
        );
        assert_eq!(openai_messages.len(), original_len);
        assert_eq!(openai_messages[1].id(), reminder_id);
        let ConversationMessage::User { content, .. } = &openai_messages[0] else {
            panic!("expected original tool result");
        };
        let ContentBlock::ToolResult { content, .. } = &content[0] else {
            panic!("expected tool result block");
        };
        assert_eq!(content.string_at(""), Some("tool output"));

        let mut anthropic_messages = make_messages();
        apply_anthropic_tool_result_reminder_suffixes(
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            &mut anthropic_messages,
            std::slice::from_ref(&suffix),
        );
        assert_eq!(anthropic_messages.len(), 1);
        let ConversationMessage::User { content, .. } = &anthropic_messages[0] else {
            panic!("expected decorated tool result");
        };
        let ContentBlock::ToolResult { content, .. } = &content[0] else {
            panic!("expected decorated tool result block");
        };
        assert_eq!(
            content.string_at(""),
            Some("tool output\n\n<system-reminder>tokens</system-reminder>")
        );
    }
}

#[cfg(test)]
mod subscription_rate_limit_tests {
    /// REGRESSION: a ChatGPT-login 429 burned the full retry ladder.
    ///
    /// The rule "a subscription's 429 is terminal" already existed, but only in
    /// Claude.ai vocabulary (`RetryState::is_subscriber`, fed from a snapshot
    /// whose `subscription_type` is an Anthropic concept). An OpenAI plan can
    /// never set it, so the ChatGPT profile fell through to the API-key path and
    /// retried a quota that resets on the plan's clock, not ours.
    #[test]
    fn a_subscription_profile_fails_fast_while_an_api_key_retries() {
        use crate::service::retry::rate_limit_cannot_clear as cannot_clear;
        use crate::AuthStrategy;
        // The credential that motivated this: ChatGPT OAuth, a plan quota.
        assert!(cannot_clear(AuthStrategy::ChatGptPlan, "gpt-6-astra"));
        // The same MODEL on an API key keeps the ladder — the model is not the
        // discriminator, the credential is.
        assert!(!cannot_clear(AuthStrategy::ApiKey, "gpt-6-astra"));
        // Unchanged: OpenRouter's free tier, matched on the model suffix.
        assert!(cannot_clear(AuthStrategy::ApiKey, "z-ai/glm-4.6:free"));
        assert!(!cannot_clear(AuthStrategy::ApiKey, "z-ai/glm-4.6"));
        // Anthropic is untouched here on purpose: its subscription is already
        // handled by the parity subscriber gate.
        assert!(!cannot_clear(AuthStrategy::OAuthBearer, "claude-opus-5"));
        assert!(!cannot_clear(AuthStrategy::None, "claude-opus-5"));
    }
}
