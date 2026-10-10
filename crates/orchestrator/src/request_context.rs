//! Request-only prompt context, owned by the invoking turn future.
//!
//! These rows are projected next to their admitted user prompt after history
//! preparation. They never enter session history, persistence, or compaction.

use crate::{ConversationOrchestrator, OrchestratorError, TurnOutcome};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use tokio_util::sync::CancellationToken;

struct PromptContext {
    prompt_id: MessageId,
    rows: Vec<ConversationMessage>,
}

tokio::task_local! {
    static REQUEST_CONTEXT: PromptContext;
}

impl ConversationOrchestrator {
    /// Run a prompt with model-only context immediately before its user row.
    /// The scope follows this future through queue admission, retries, and
    /// fallback; cancellation or dropping the future cannot leave shared rows.
    pub async fn run_turn_streaming_with_request_context(
        &self,
        prompt: impl Into<Utf16JsonProjection>,
        context: Vec<Utf16JsonProjection>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let prompt_id = MessageId::new();
        let rows = context_rows(context);
        REQUEST_CONTEXT
            .scope(
                PromptContext { prompt_id, rows },
                self.run_turn_streaming_with_cancel_image_sources_and_message_id(
                    prompt,
                    Vec::new(),
                    cancel,
                    Some(prompt_id),
                ),
            )
            .await
    }

    /// Batched counterpart using the same future-owned, nonpersistent context.
    pub async fn run_turn_with_request_context(
        &self,
        prompt: impl Into<Utf16JsonProjection>,
        context: Vec<Utf16JsonProjection>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let prompt_id = MessageId::new();
        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id,
                    rows: context_rows(context),
                },
                self.run_turn_with_cancel_image_sources_and_message_id(
                    prompt,
                    Vec::new(),
                    cancel,
                    Some(prompt_id),
                ),
            )
            .await
    }
}

fn context_rows(context: Vec<Utf16JsonProjection>) -> Vec<ConversationMessage> {
    context
        .into_iter()
        .map(|text| {
            let mut row = ConversationMessage::user_meta(MessageId::new(), String::new());
            if let ConversationMessage::User { content, .. } = &mut row {
                *content = vec![ContentBlock::TextJsUtf16 {
                    text: text.string_at("").unwrap_or_default().to_owned(),
                    utf16_code_units: text.string_units("").unwrap_or_default(),
                    citations: None,
                }];
            }
            row
        })
        .collect()
}

/// Attach only when the corresponding real user row survived preparation.
/// Re-applying to an already prepared snapshot preserves each context row once.
pub(crate) fn attach(messages: &mut Vec<ConversationMessage>) {
    let _ = REQUEST_CONTEXT.try_with(|context| {
        let ids = context
            .rows
            .iter()
            .map(ConversationMessage::id)
            .collect::<std::collections::HashSet<_>>();
        messages.retain(|row| !ids.contains(&row.id()));
        if let Some(index) = messages
            .iter()
            .position(|row| row.id() == context.prompt_id)
        {
            messages.splice(index..index, context.rows.iter().cloned());
        }
    });
}
