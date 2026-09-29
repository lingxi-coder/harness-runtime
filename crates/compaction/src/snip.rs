//! Snip compaction — the cheapest layer. Drops oldest messages with no LLM
//! involvement. Always preserves the most-recent `MIN_PROTECTED_TAIL` messages.

use lingxi_core::types::ConversationMessage;

/// Result of running the snip compactor.
#[derive(Debug, Clone)]
pub struct SnipResult {
    /// Surviving messages, oldest-first, with the dropped prefix removed.
    pub messages: Vec<ConversationMessage>,
    /// Approximate tokens freed by dropping the prefix.
    pub tokens_freed: u64,
    /// Number of messages dropped from the head.
    pub removed_count: usize,
}

/// Stateless snip compactor — no configuration, no LLM.
pub struct SnipCompactor;

const MIN_PROTECTED_TAIL: usize = 10;

impl SnipCompactor {
    /// Drop oldest messages until under budget. No LLM.
    #[must_use]
    pub fn snip(
        messages: Vec<ConversationMessage>,
        current_tokens: u64,
        budget: u64,
    ) -> SnipResult {
        if current_tokens <= budget {
            return SnipResult {
                messages,
                tokens_freed: 0,
                removed_count: 0,
            };
        }
        let mut snipped = messages;
        let mut freed = 0u64;
        let mut removed = 0usize;
        while current_tokens.saturating_sub(freed) > budget && snipped.len() > MIN_PROTECTED_TAIL {
            let m = snipped.remove(0);
            freed = freed.saturating_add(estimate_tokens(&m));
            removed += 1;
        }
        SnipResult {
            messages: snipped,
            tokens_freed: freed,
            removed_count: removed,
        }
    }
}

fn estimate_tokens(m: &ConversationMessage) -> u64 {
    // Rough: 4 chars per token.
    u64::try_from(m.text_content().len()).unwrap_or(u64::MAX) / 4
}
