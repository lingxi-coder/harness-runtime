//! The `EndConversation` tool (new in claude-code 2.1.206) — lets the model end
//! the current conversation in extreme cases of sustained abuse (or a requested
//! demonstration). Gated behind a model floor + the `tengu_umber_kestrel` GB
//! flag; DEFAULT OFF, so the composition root only registers it when enabled.
//!
//! Lives in `orchestrator` (which owns `tool-api`) alongside the byte-locked
//! strings in [`crate::prompt::end_conversation`]. This is CHUNK A — the tool
//! DEFINITION (metadata + schema + a `call` that reports the end result). The
//! two-call confirmation flow (`lastAssistantTurnCalledEndConversation`) and the
//! actual conversation termination live in the turn loop (CHUNK B), which reads
//! the tool call and decides whether to end (2nd consecutive call) or return the
//! re-read reminder (1st call).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use serde_json::{Value, json};
use tool_api::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolProgressSender,
    ToolStaticContext, ToolUseContext,
};

use crate::prompt::end_conversation as ec;

/// Shared flag the tool raises when a SECOND consecutive `EndConversation` call
/// actually ends the conversation. The turn loop reads it after tool execution
/// to terminate + emit [`ec::END_CONVERSATION_ENDED_MESSAGE`]. Mirrors
/// `StructuredOutputTool`'s captured-slot pattern (no shared-struct change).
pub type EndConversationSlot = Arc<AtomicBool>;

/// The `EndConversation` tool. `enabled` mirrors claude-code
/// `isEndConversationToolEnabled` (model floor `NWn` && GB `tengu_umber_kestrel`)
/// — the composition root computes it; DEFAULT `false` keeps the tool
/// unregistered/inert so the prompt + tool set stay byte-identical.
pub struct EndConversationTool {
    schema: Value,
    enabled: bool,
    /// Raised by [`Self::call`] when the conversation is actually ended (the
    /// 2nd consecutive call). The turn loop consumes it.
    end_requested: EndConversationSlot,
}

impl EndConversationTool {
    /// Build the tool. `enabled` = the resolved `isEndConversationToolEnabled`
    /// signal (default OFF); `end_requested` is the slot the turn loop reads.
    #[must_use]
    pub fn new(enabled: bool, end_requested: EndConversationSlot) -> Self {
        Self {
            schema: ec::input_schema(),
            enabled,
            end_requested,
        }
    }

    /// The shared end-request slot (for the composition root / turn loop).
    #[must_use]
    pub fn slot(&self) -> EndConversationSlot {
        self.end_requested.clone()
    }
}

impl Default for EndConversationTool {
    fn default() -> Self {
        Self::new(false, Arc::new(AtomicBool::new(false)))
    }
}

/// claude-code `lastAssistantTurnCalledEndConversation`: did the assistant turn
/// PRECEDING the current one call `EndConversation`? The current (in-flight)
/// assistant message is the last `Assistant` entry in `messages`; this checks
/// the one before it. A `true` result means the incoming call is the SECOND
/// consecutive one → actually end (vs. the 1st → return the re-read reminder).
#[must_use]
fn prior_assistant_turn_called_end_conversation(messages: &[ConversationMessage]) -> bool {
    let assistant_calls: Vec<bool> = messages
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::Assistant { content, .. } => Some(content.iter().any(|b| {
                matches!(b, ContentBlock::ToolUse { name, .. }
                    if name == ec::END_CONVERSATION_TOOL_NAME)
            })),
            _ => None,
        })
        .collect();
    // The last assistant entry is the current turn; look at the one before it.
    assistant_calls.len() >= 2 && assistant_calls[assistant_calls.len() - 2]
}

#[async_trait]
impl Tool for EndConversationTool {
    fn name(&self) -> &str {
        ec::END_CONVERSATION_TOOL_NAME
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // `isEndConversationToolEnabled` (model-floor && umber_kestrel GB).
        // DEFAULT OFF → tool absent → byte-identical prompt + tool set.
        self.enabled
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // claude-code `isReadOnly(){return!0}`.
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // claude-code `isConcurrencySafe(){return!1}`.
        false
    }

    fn max_result_size_chars(&self) -> usize {
        ec::END_CONVERSATION_MAX_RESULT_SIZE_CHARS
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        // claude-code `checkPermissions(){return{behavior:"allow",updatedInput}}`.
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "end conversation".to_string(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        // claude-code `description(){return G2r}` — the full text.
        ec::render_prompt(ec::END_CONVERSATION_TOOL_NAME)
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        // claude-code `prompt(){return G2r}` — same full text as description().
        ec::render_prompt(ec::END_CONVERSATION_TOOL_NAME)
    }

    async fn call(
        &self,
        _input: Value,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let tool = ec::END_CONVERSATION_TOOL_NAME;
        if ctx.agent_id.is_some() {
            // FORK branch (206 checks `t.agentId` FIRST): a background fork can
            // end neither the main conversation nor itself. Return the fork
            // reflection prompt with ended:false and NEVER raise the slot.
            return Ok(ToolCallResult {
                data: json!({
                    "ended": false,
                    "message": ec::END_CONVERSATION_FORK_REFLECTION_PROMPT,
                }),
                model_content: Some(ec::END_CONVERSATION_FORK_REFLECTION_PROMPT.to_string()),
                new_messages: Vec::new(),
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            });
        }
        if prior_assistant_turn_called_end_conversation(&ctx.messages) {
            // SECOND consecutive call → actually end. Raise the slot so the turn
            // loop terminates + surfaces the user-facing finalMessage (k4i). The
            // tool's own `message` returned to the MODEL is the tool RESULT
            // (MWn), NOT the finalMessage — matching 206's
            // `{data:{ended:true, message: MWn}}` + `finalMessage: k4i`.
            self.end_requested.store(true, Ordering::SeqCst);
            Ok(ToolCallResult {
                data: json!({
                    "ended": true,
                    "message": ec::END_CONVERSATION_TOOL_RESULT,
                }),
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        } else {
            // FIRST call → do NOT end; return the re-read reminder (guidance
            // appended) so the model must confirm by calling again.
            let reminder = ec::render_reread_reminder(tool, &ec::render_prompt(tool));
            Ok(ToolCallResult {
                data: json!({ "ended": false, "message": reminder }),
                model_content: Some(reminder),
                new_messages: Vec::new(),
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::MessageId;

    fn tool() -> EndConversationTool {
        EndConversationTool::new(true, Arc::new(AtomicBool::new(false)))
    }

    fn asst_calling_endconv() -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: Default::default(),
                name: "EndConversation".into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    fn ctx_with(messages: Vec<ConversationMessage>) -> ToolUseContext {
        let mut c = ToolUseContext::model_seed("x".into(), None);
        c.messages = messages;
        c
    }

    #[test]
    fn default_is_disabled_and_shaped() {
        let t = EndConversationTool::default();
        assert_eq!(t.name(), "EndConversation");
        assert!(!t.is_enabled(&ToolStaticContext::default()));
        assert!(t.should_defer());
        assert!(t.is_read_only(&json!({})));
        assert!(!t.is_concurrency_safe(&json!({})));
        assert_eq!(t.max_result_size_chars(), 10_000);
        assert_eq!(t.input_schema()["additionalProperties"], false);
    }

    #[tokio::test]
    async fn description_and_prompt_are_the_full_g2r() {
        let t = tool();
        let d = t
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert!(t.is_enabled(&ToolStaticContext::default()));
        assert!(d.starts_with("End the current conversation. Use only for sustained user abuse"));
        assert!(d.ends_with("and then use the EndConversation tool to do so."));
        assert_eq!(d, t.prompt(&PromptOptions::default()).await);
    }

    #[tokio::test]
    async fn first_call_returns_reminder_and_does_not_end() {
        // Only the current turn calls EndConversation (no prior) → 1st call.
        let t = tool();
        let (tx, _rx) = tool_api::progress::progress_channel();
        let r = t
            .call(json!({}), ctx_with(vec![asst_calling_endconv()]), tx)
            .await
            .expect("ok");
        assert_eq!(r.data["ended"], false);
        assert!(r
            .model_content
            .as_deref()
            .unwrap()
            .starts_with("Re-read the EndConversation tool guidance below."));
        assert!(
            !t.end_requested.load(Ordering::SeqCst),
            "must NOT end on 1st call"
        );
    }

    #[tokio::test]
    async fn second_consecutive_call_ends_and_raises_slot() {
        // A prior assistant turn ALSO called EndConversation → 2nd consecutive.
        let t = tool();
        let (tx, _rx) = tool_api::progress::progress_channel();
        let msgs = vec![
            asst_calling_endconv(), // prior turn
            ConversationMessage::user(MessageId::new(), "reminder ack".into()),
            asst_calling_endconv(), // current turn
        ];
        let r = t.call(json!({}), ctx_with(msgs), tx).await.expect("ok");
        assert_eq!(r.data["ended"], true);
        // Tool RESULT (MWn), returned to the model — NOT the user-facing
        // finalMessage (k4i, emitted separately by the turn loop).
        assert_eq!(r.data["message"], "Claude has ended this chat.");
        assert!(
            t.end_requested.load(Ordering::SeqCst),
            "slot raised on 2nd call"
        );
    }

    #[tokio::test]
    async fn fork_call_never_ends_and_returns_fork_reflection() {
        // agent_id set (background fork) → 206 branches on `t.agentId` FIRST:
        // even a 2nd-consecutive call must NOT end; returns x4i with ended:false.
        let t = tool();
        let (tx, _rx) = tool_api::progress::progress_channel();
        let mut ctx = ctx_with(vec![
            asst_calling_endconv(),
            ConversationMessage::user(MessageId::new(), "ack".into()),
            asst_calling_endconv(),
        ]);
        ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        let r = t.call(json!({}), ctx, tx).await.expect("ok");
        assert_eq!(
            r.data["ended"], false,
            "a fork can never end the conversation"
        );
        assert!(r.data["message"]
            .as_str()
            .unwrap()
            .starts_with("You are running as a background fork"));
        assert!(
            !t.end_requested.load(Ordering::SeqCst),
            "fork must NOT raise the slot"
        );
    }
}
