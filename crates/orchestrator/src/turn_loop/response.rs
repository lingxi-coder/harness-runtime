use super::error_reporting::surface_terminal_api_error;
use super::{
    RecoveryState, TurnStepOutcome, ESCALATED_MAX_TOKENS, MALFORMED_TOOL_USE_RETRY_FAILED,
    MALFORMED_TOOL_USE_RETRY_NUDGE, MAX_OUTPUT_TOKENS_RECOVERY_LIMIT,
    MAX_OUTPUT_TOKENS_RECOVERY_NUDGE, STRUCTURED_OUTPUT_TOOL_NAME, THINKING_ONLY_NUDGE,
};
use crate::conversation::{ApiErrorEnvelope, ConversationOrchestrator};
use crate::error::OrchestratorError;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use llm_runtime::ContentBlock as LlmContentBlock;

/// A1 `max_tokens` recovery decision (TS `query.ts:1223-1255`).
///
/// While `count < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT`: append the byte-exact
/// meta nudge user message to history, increment the counter, and Continue.
/// On exhaustion (count has reached the limit): end the turn with
/// `stop_reason = "max_tokens"` (current behavior — surface the cap).
///
/// The 8k→64k escalation (TS `query.ts:1199-1221`) fires FIRST when
/// [`crate::OrchestratorConfig::escalate_max_output_tokens`] is on and it has
/// not yet fired this episode: it arms the override and returns `Continue` so
/// the SAME step re-issues once at [`ESCALATED_MAX_TOKENS`] with no nudge.
pub(super) async fn handle_max_output_tokens(
    orch: &ConversationOrchestrator,
    assistant_id: MessageId,
    state: &mut RecoveryState,
) -> Result<TurnStepOutcome, OrchestratorError> {
    // REC.A1 escalation (8k→64k). TS (`query.ts:1199-1221`) does a single-shot
    // retry at the escalated cap BEFORE the multi-turn nudge, gated on
    // `tengu_otk_slot_v1` (here `escalate_max_output_tokens`) and "not already
    // escalated". We arm `max_output_tokens_override` — which the next
    // `execute_one_turn_with_recovery_tracked` TAKEs and passes to
    // `messages_create` — and return `Continue` so the same step
    // re-issues at 64k with NO nudge injected. The override is taken per call,
    // so a separate `max_output_tokens_escalated` flag (reset alongside the
    // recovery count) gates this to once per episode and prevents an
    // escalate-forever loop when 64k also overflows.
    if orch.config.escalate_max_output_tokens && !state.max_output_tokens_escalated {
        state.max_output_tokens_override = Some(ESCALATED_MAX_TOKENS);
        state.max_output_tokens_escalated = true;
        return Ok(TurnStepOutcome::Continue);
    }

    if state.max_output_tokens_recovery_count < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT {
        // Inject the "resume directly" nudge as a META user message. CC 2.1.207
        // builds it via `createUserMessage({…, isMeta:!0})`, so it persists with
        // top-level `isMeta:true` and is skipped by title / first-prompt /
        // visible-count extraction.
        let nudge_msg = ConversationMessage::user_meta(
            MessageId::new(),
            MAX_OUTPUT_TOKENS_RECOVERY_NUDGE.to_string(),
        );
        {
            let mut s = orch.session.lock().await;
            s.history.push(nudge_msg.clone());
        }
        orch.persist_message_to_jsonl(&nudge_msg).await;

        state.max_output_tokens_recovery_count =
            state.max_output_tokens_recovery_count.saturating_add(1);
        // A clean nudge retry never carries an escalated override forward
        // (TS sets `maxOutputTokensOverride: undefined` here).
        state.max_output_tokens_override = None;
        return Ok(TurnStepOutcome::Continue);
    }

    // Recovery exhausted — surface the byte-locked `API Error: …` cap message
    // (the streaming twin does this in its terminal arm), then end the turn.
    let surfaced_id = surface_terminal_api_error(orch, "max_tokens", None).await;
    Ok(TurnStepOutcome::Ended {
        final_message_id: surfaced_id.unwrap_or(assistant_id),
        stop_reason: "max_tokens".to_string(),
        allow_budget_continuation: false,
        tool_requested_end: false,
    })
}

/// Whether any assistant content block is a non-whitespace [`ContentBlock::Text`]
/// — the #78 "visible output" predicate (claude-code `bin/claude.exe` offset
/// ~202946760). `false` = a thinking-only / text-empty response.
pub(super) fn has_visible_text(blocks: &[ContentBlock]) -> bool {
    blocks
        .iter()
        .any(|block| block.visible_text().is_some_and(|text| !text.trim().is_empty()))
}

/// Port of claude-code's `Pt(ce)` (query module, `bin/claude.exe` offset
/// ~209123866): scanning the message history backward, return `true` when the
/// most recent assistant carried a `StructuredOutput` `tool_use` BEFORE any real
/// user turn. Meta user messages and tool-result-carrier user messages
/// (`Jde(e)` = a `user` message whose content array holds a `tool_result`) are
/// skipped; a real user message short-circuits to `false`.
///
/// Used as the `!Pt(ce)` guard on the #78 thinking-only nudge: in a
/// structured-output exchange the model's post-`StructuredOutput` `end_turn`
/// legitimately carries no visible text, so the "[Your previous response had no
/// visible output…]" nudge must NOT fire.
#[must_use]
pub(crate) fn prior_assistant_used_structured_output(history: &[ConversationMessage]) -> bool {
    for msg in history.iter().rev() {
        match msg.role() {
            lingxi_core::types::MessageRole::User => {
                // `if(Sn.isMeta||Jde(Sn))continue; return!1`
                if msg.is_meta() || is_tool_result_carrier(msg) {
                    continue;
                }
                return false;
            }
            lingxi_core::types::MessageRole::Assistant => {
                // `Sn.message.content.some(b=>b.type==="tool_use"&&b.name===bp)`
                if msg.tool_calls().iter().any(|b| {
                    matches!(b, ContentBlock::ToolUse { name, .. } if name == STRUCTURED_OUTPUT_TOOL_NAME)
                }) {
                    return true;
                }
            }
            // `if(Sn.type!=="assistant")continue` — system / other lines skipped.
            lingxi_core::types::MessageRole::System => continue,
        }
    }
    false
}

/// claude-code `Jde(e)`: a `user` message whose content array contains any
/// `tool_result` block (a synthetic tool-result-carrier turn, not a real human
/// turn).
pub(super) fn is_tool_result_carrier(msg: &ConversationMessage) -> bool {
    matches!(
        msg,
        ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    )
}

/// #77 (batched twin, claude-code `bin/claude.exe` offset ~202945837): handle a
/// `stop_reason == "tool_use"` response that produced ZERO `tool_use` blocks. On
/// the FIRST failure inject the byte-exact meta retry nudge, reset the
/// max-output-tokens recovery bookkeeping, arm the per-turn guard, and Continue.
/// On the SECOND, surface the non-meta terminal message and end the turn as
/// completed (`stop_reason = "end_turn"`).
pub(super) async fn handle_malformed_tool_use(
    orch: &ConversationOrchestrator,
    assistant_id: MessageId,
    state: &mut RecoveryState,
) -> Result<TurnStepOutcome, OrchestratorError> {
    if state.malformed_tool_use_retried {
        // Second failure → terminal NON-meta message, complete the turn. The
        // binary builds this via `ql(...)`→`mcc({isApiErrorMessage:!0})`, i.e. an
        // ASSISTANT api-error message (`role:"assistant", stop_reason:
        // "stop_sequence", stop_details:null`) appended AFTER the malformed
        // assistant response — two assistant messages in a row, matching the
        // binary. (The port previously persisted a USER message here.) Shape
        // mirrors `surface_model_error`'s assistant-api-error message.
        orch.output.emit_text(MALFORMED_TOOL_USE_RETRY_FAILED).await;
        let failed_msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: MALFORMED_TOOL_USE_RETRY_FAILED.to_string(), citations: None,
            }],
            stop_reason: Some("stop_sequence".to_string()),
        };
        {
            let mut s = orch.session.lock().await;
            s.history.push(failed_msg.clone());
        }
        // claude-code builds the terminal via `ql({content})` with no `error:`
        // arg → `isApiErrorMessage: true`, `error`/`apiErrorStatus` OMITTED,
        // inner `stop_reason:"stop_sequence"`.
        orch.persist_api_error_message_to_jsonl(&failed_msg, ApiErrorEnvelope::default())
            .await;
        return Ok(TurnStepOutcome::Ended {
            final_message_id: failed_msg.id(),
            stop_reason: "end_turn".to_string(),
            allow_budget_continuation: false,
            tool_requested_end: false,
        });
    }
    let nudge_msg = ConversationMessage::user_meta(
        MessageId::new(),
        MALFORMED_TOOL_USE_RETRY_NUDGE.to_string(),
    );
    orch.discard_retry_attempt(assistant_id).await;
    {
        let mut s = orch.session.lock().await;
        s.history.push(nudge_msg.clone());
    }
    orch.persist_message_to_jsonl(&nudge_msg).await;
    // TS resets the recovery counters on the retry transition.
    state.reset_max_output_tokens_recovery();
    state.malformed_tool_use_retried = true;
    Ok(TurnStepOutcome::Continue)
}

/// #78 (batched twin, claude-code `bin/claude.exe` offset ~202946760): inject the
/// once-per-turn thinking-only nudge as a meta user message and Continue. Caller
/// has already checked `!thinking_only_nudged && !has_visible_text(..)`.
pub(super) async fn handle_thinking_only(
    orch: &ConversationOrchestrator,
    assistant_id: MessageId,
    state: &mut RecoveryState,
) -> Result<TurnStepOutcome, OrchestratorError> {
    let nudge_msg =
        ConversationMessage::user_meta(MessageId::new(), THINKING_ONLY_NUDGE.to_string());
    orch.discard_retry_attempt(assistant_id).await;
    {
        let mut s = orch.session.lock().await;
        s.history.push(nudge_msg.clone());
    }
    orch.persist_message_to_jsonl(&nudge_msg).await;
    state.thinking_only_nudged = true;
    Ok(TurnStepOutcome::Continue)
}

/// Translate llm-runtime content blocks into protocol content blocks.
/// Server-side variants (`RedactedThinking`, `ServerToolUse`, `ConnectorText`,
/// `AdvisorToolResult`) are PRESERVED verbatim (not dropped) so resume/replay
/// JSONL bytes stay intact when the protected-thinking/advisor/connector betas
/// are active. `ToolCall.id: String` becomes the canonical String-backed
/// `ToolUseId` directly (the provider id IS the id; no UUID round-trip), so
/// JSONL/resume bytes match upstream claude-code. Input-only variants
/// (`Image`/`ImageUrl`/`Document`/…) remain dropped on the response path.
#[must_use]
pub(crate) fn translate_response_blocks(content: &[LlmContentBlock]) -> Vec<ContentBlock> {
    use lingxi_core::types::ToolUseId;
    content
        .iter()
        .filter_map(|b| match b {
            LlmContentBlock::ProviderContent { protocol, value } => Some(ContentBlock::ProviderContent { protocol: protocol.clone(), value: value.clone() }),
            LlmContentBlock::Text { text, citations, .. } => Some(ContentBlock::Text {
                text: text.clone(),
                citations: citations.clone(),
            }),
            LlmContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
                ..
            } => Some(ContentBlock::TextJsUtf16 {
                text: text.clone(),
                utf16_code_units: utf16_code_units.clone(),
                citations: citations.clone(),
            }),
            LlmContentBlock::ToolCall { id, name, input } => {
                // The provider-issued id (e.g. Anthropic `toolu_…`, OpenAI
                // `call_…`) IS the canonical `ToolUseId`, so JSONL/resume bytes
                // match upstream claude-code. The `provider_id` sidecar is left
                // `None` (vestigial) — the id already carries the canonical value.
                //
                // (cc 2.1.218 `jYd`) Repair literal `\uXXXX` TEXT the model
                // emitted instead of real characters, before the input is stored
                // or dispatched — otherwise `Edit.old_string` never matches and
                // paths don't resolve. Windows paths and genuinely-escaped
                // sequences are left verbatim; `Workflow.script` is restored.
                let (input, _stats) =
                    llm_runtime::unicode_repair::repair_tool_input(name, input);
                Some(ContentBlock::ToolUse {
                    id: ToolUseId::from(id.clone()),
                    name: name.clone(),
                    input,
                    provider_id: None,
                })
            }
            LlmContentBlock::Reasoning { text, signature } => Some(ContentBlock::Thinking {
                thinking: text.clone(),
                signature: signature.clone(),
            }),
            // Low-frequency server-side blocks: PRESERVED verbatim so resume/replay
            // JSONL bytes stay intact when protected-thinking/advisor/connector
            // betas are active (matches agent::runner::translate_response_blocks).
            // Output-only — claude-code keeps them; non-streaming twin of the
            // streaming `event_router`.
            LlmContentBlock::RedactedThinking { data } => {
                Some(ContentBlock::RedactedThinking { data: data.clone() })
            }
            LlmContentBlock::ServerToolUse { id, name, input } => {
                Some(ContentBlock::ServerToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            }
            LlmContentBlock::ConnectorText {
                connector_text,
                signature,
            } => Some(ContentBlock::ConnectorText {
                connector_text: connector_text.clone(),
                signature: signature.clone(),
            }),
            LlmContentBlock::AdvisorToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(ContentBlock::AdvisorToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            // Input-only / non-output variants remain dropped on the response path.
            LlmContentBlock::Image { .. }
            | LlmContentBlock::ImageUrl { .. }
            | LlmContentBlock::Document { .. }
            | LlmContentBlock::ToolResult { .. }
            // cache_edits is a request-only directive — never in a response.
            | LlmContentBlock::CacheEdits { .. } => None,
        })
        .collect()
}
