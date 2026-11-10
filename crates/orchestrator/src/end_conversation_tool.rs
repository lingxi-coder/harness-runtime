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

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use serde_json::json;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, source_text_projection, DescriptionOptions,
    PromptOptions, ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance,
    ToolStaticContext,
};
use tool_api::{Tool, ToolCallResult, ToolError, ToolProgressSender, ToolUseContext};

use crate::prompt::end_conversation as ec;

/// Shared flag the tool raises when a SECOND consecutive `EndConversation` call
/// actually ends the conversation. The turn loop reads it after tool execution
/// to terminate + emit [`ec::END_CONVERSATION_ENDED_MESSAGE`]. Mirrors
/// `StructuredOutputTool`'s captured-slot pattern (no shared-struct change).
pub type EndConversationSlot = Arc<AtomicBool>;

#[derive(Clone)]
pub(crate) struct EndConversationRequest {
    pub(crate) slot: EndConversationSlot,
    pub(crate) publication_guard: Option<Arc<dyn hooks::attachment::HookPublicationGuard>>,
}

tokio::task_local! {
    static DISPATCH_END_REQUEST: Arc<std::sync::Mutex<Option<EndConversationSlot>>>;
}

/// A dispatcher must publish termination with its selected result. Direct tool
/// callers retain the captured-slot API, while dispatched calls leave the slot
/// untouched until the result's publication and consumption boundaries.
pub(crate) async fn capture_end_conversation_request<F: std::future::Future>(
    tool_name: &str,
    future: F,
) -> (F::Output, Option<EndConversationSlot>) {
    if tool_name != ec::END_CONVERSATION_TOOL_NAME {
        return (future.await, None);
    }
    let request = Arc::new(std::sync::Mutex::new(None));
    let output = DISPATCH_END_REQUEST.scope(request.clone(), future).await;
    let captured = request
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    (output, captured)
}

/// The `EndConversation` tool. `enabled` mirrors claude-code
/// `isEndConversationToolEnabled` (model floor `NWn` && GB `tengu_umber_kestrel`)
/// — the composition root computes it; DEFAULT `false` keeps the tool
/// unregistered/inert so the prompt + tool set stay byte-identical.
pub struct EndConversationTool {
    schema: Utf16JsonProjection,
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
            schema: generated_builtin_schema(&ec::input_schema()),
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
/// PRECEDING the current one call `EndConversation`? Normal model dispatch
/// supplies request history separately from the current response. Recovery can
/// include that response in history, so its identity must be excluded. A `true`
/// result means the incoming call is the SECOND consecutive one → actually end.
#[must_use]
fn prior_assistant_turn_called_end_conversation(
    messages: &[ConversationMessage],
    current_assistant_id: Option<lingxi_core::types::MessageId>,
) -> bool {
    messages
        .iter()
        .rev()
        .filter(|message| Some(message.id()) != current_assistant_id)
        .find_map(|m| match m {
            ConversationMessage::Assistant { content, .. } => Some(content.iter().any(|b| {
                matches!(b, ContentBlock::ToolUse { name, .. }
                    if name == ec::END_CONVERSATION_TOOL_NAME)
            })),
            _ => None,
        })
        .unwrap_or(false)
}

#[async_trait]
impl Tool for EndConversationTool {
    fn name(&self) -> &str {
        ec::END_CONVERSATION_TOOL_NAME
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: Some(HashSet::new()),
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // `isEndConversationToolEnabled` (model-floor && umber_kestrel GB).
        // DEFAULT OFF → tool absent → byte-identical prompt + tool set.
        self.enabled
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        // claude-code `isReadOnly(){return!0}`.
        true
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        // claude-code `isConcurrencySafe(){return!1}`.
        false
    }

    fn max_result_size_chars(&self) -> usize {
        ec::END_CONVERSATION_MAX_RESULT_SIZE_CHARS
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        // claude-code `checkPermissions(){return{behavior:"allow",updatedInput}}`.
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "end conversation".into(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        // claude-code `description(){return G2r}` — the full text.
        ec::render_prompt(ec::END_CONVERSATION_TOOL_NAME)
    }

    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        // claude-code `prompt(){return G2r}` — same full text as description().
        ec::render_prompt(ec::END_CONVERSATION_TOOL_NAME).into()
    }

    async fn call(
        &self,
        _input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let tool = ec::END_CONVERSATION_TOOL_NAME;
        if ctx.agent_id.is_some() {
            // FORK branch (206 checks `t.agentId` FIRST): a background fork can
            // end neither the main conversation nor itself. Return the fork
            // reflection prompt with ended:false and NEVER raise the slot.
            let data = generated_builtin_result(&json!({
                "ended": false,
                "message": ec::END_CONVERSATION_FORK_REFLECTION_PROMPT,
            }));
            let mut result = ToolCallResult::from_data(data);
            result.model_content = Some(source_text_projection(
                ec::END_CONVERSATION_FORK_REFLECTION_PROMPT,
            ));
            return Ok(result);
        }
        if prior_assistant_turn_called_end_conversation(&ctx.messages, ctx.assistant_message_id) {
            // SECOND consecutive call → actually end. Raise the slot so the turn
            // loop terminates + surfaces the user-facing finalMessage (k4i). The
            // tool's own `message` returned to the MODEL is the tool RESULT
            // (MWn), NOT the finalMessage — matching 206's
            // `{data:{ended:true, message: MWn}}` + `finalMessage: k4i`.
            if DISPATCH_END_REQUEST
                .try_with(|request| {
                    *request
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(self.end_requested.clone());
                })
                .is_err()
            {
                self.end_requested.store(true, Ordering::SeqCst);
            }
            Ok(ToolCallResult::from_data(generated_builtin_result(
                &json!({
                    "ended": true,
                    "message": ec::END_CONVERSATION_TOOL_RESULT,
                }),
            )))
        } else {
            // FIRST call → do NOT end; return the re-read reminder (guidance
            // appended) so the model must confirm by calling again.
            let reminder = ec::render_reread_reminder(tool, &ec::render_prompt(tool));
            let data = generated_builtin_result(&json!({
                "ended": false,
                "message": reminder,
            }));
            let mut result = ToolCallResult::from_data(data);
            result.model_content = Some(source_text_projection(&reminder));
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::MessageId;

    fn source_json(input: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(input).expect("valid source fixture")
    }

    fn tool() -> EndConversationTool {
        EndConversationTool::new(true, Arc::new(AtomicBool::new(false)))
    }

    fn asst_calling_endconv() -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: Default::default(),
                name: "EndConversation".into(),
                input: Utf16JsonProjection::from_serde_value(&json!({}))
                    .expect("empty tool input is valid JSON"),
                provider_id: None,
            }],
            stop_reason: None,
            resumable: None,
        }
    }

    fn ctx_with(messages: Vec<ConversationMessage>) -> ToolUseContext {
        let mut c = ToolUseContext::model_seed("x".into());
        c.messages = messages;
        c.assistant_message = Some(asst_calling_endconv());
        c
    }

    #[test]
    fn default_is_disabled_and_shaped() {
        let t = EndConversationTool::default();
        assert_eq!(t.name(), "EndConversation");
        assert!(!t.is_enabled(&ToolStaticContext::default()));
        assert!(t.should_defer());
        let input = source_json("{}");
        assert!(t.is_read_only(&input));
        assert!(!t.is_concurrency_safe(&input));
        assert_eq!(t.max_result_size_chars(), 10_000);
        assert_eq!(
            t.input_schema()
                .get_property("additionalProperties")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[tokio::test]
    async fn description_and_prompt_are_the_full_g2r() {
        let t = tool();
        let d = t
            .description(
                &source_json("{}"),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert!(t.is_enabled(&ToolStaticContext::default()));
        assert!(d.starts_with("End the current conversation. Use only for sustained user abuse"));
        assert!(d.ends_with("and then use the EndConversation tool to do so."));
        let prompt = t.prompt(&PromptOptions::default()).await;
        assert_eq!(
            d.as_str(),
            prompt.string_at("").expect("prompt is a root string")
        );
    }

    #[tokio::test]
    async fn permission_allow_echoes_exact_updated_input() {
        let t = tool();
        let input = source_json(r#"{"value":"left\ud800right"}"#);
        let result = t.check_permissions(&input, &ctx_with(Vec::new())).await;
        let permission::PermissionResult::Allow {
            updated_input: Some(updated),
            ..
        } = result
        else {
            panic!("EndConversation always allows and echoes updated input")
        };
        assert_eq!(
            updated.string_units("/value"),
            Some(vec![
                b'l' as u16,
                b'e' as u16,
                b'f' as u16,
                b't' as u16,
                0xd800,
                b'r' as u16,
                b'i' as u16,
                b'g' as u16,
                b'h' as u16,
                b't' as u16,
            ])
        );
    }

    #[tokio::test]
    async fn first_call_returns_reminder_and_does_not_end() {
        // The current assistant is separate from the request's prior history.
        let t = tool();
        let (tx, _rx) = tool_api::progress::progress_channel();
        let r = t
            .call(
                source_json("{}"),
                ctx_with(vec![ConversationMessage::user(
                    MessageId::new(),
                    "request".into(),
                )]),
                tx,
            )
            .await
            .expect("ok");
        assert_eq!(
            r.data
                .get_property("ended")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert!(r
            .model_content
            .as_ref()
            .and_then(|content| content.string_at(""))
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
        ];
        let r = t
            .call(source_json("{}"), ctx_with(msgs), tx)
            .await
            .expect("ok");
        assert_eq!(
            r.data
                .get_property("ended")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        // Tool RESULT (MWn), returned to the model — NOT the user-facing
        // finalMessage (k4i, emitted separately by the turn loop).
        assert_eq!(
            r.data
                .get_property("message")
                .and_then(|value| value.as_str()),
            Some("Claude has ended this chat.")
        );
        assert!(
            t.end_requested.load(Ordering::SeqCst),
            "slot raised on 2nd call"
        );
    }

    #[tokio::test]
    async fn intervening_assistant_without_end_resets_confirmation() {
        let t = tool();
        let (tx, _rx) = tool_api::progress::progress_channel();
        let prior_reply = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "ordinary reply".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
            per_turn_effort: None,
        };
        let r = t
            .call(
                source_json("{}"),
                ctx_with(vec![
                    asst_calling_endconv(),
                    ConversationMessage::user(MessageId::new(), "continue".into()),
                    prior_reply,
                    ConversationMessage::user(MessageId::new(), "another request".into()),
                ]),
                tx,
            )
            .await
            .expect("first confirmation after an ordinary reply");
        assert_eq!(
            r.data
                .get_property("ended")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert!(!t.end_requested.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn orphaned_end_conversation_requires_two_distinct_assistant_turns() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use lingxi_core::host::permission_gate::PermissionOutcome;
        use lingxi_core::types::SessionId;

        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let session_id = SessionId::new();
        let path = cwd.join(format!("{}.jsonl", session_id.as_uuid()));
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
        ));
        let slot = Arc::new(AtomicBool::new(false));
        let mut tools = tool_api::ToolRegistry::new();
        tools.register_builtin(Arc::new(EndConversationTool::new(true, slot.clone())));
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd,
        )
        .with_session_id(session_id)
        .with_jsonl_writer(writer)
        .with_end_conversation_slot(slot.clone())
        .without_host_environment_probes();
        orch.initialize_conversation_mode().await.unwrap();

        for expected_ended in [false, true] {
            orch.append_external_history_message(ConversationMessage::user(
                MessageId::new(),
                "request requiring confirmation".into(),
            ))
            .await;
            let current = asst_calling_endconv();
            let ConversationMessage::Assistant { content, .. } = &current else {
                unreachable!();
            };
            let ContentBlock::ToolUse { id, .. } = &content[0] else {
                unreachable!();
            };
            let id = id.clone();
            // A resumed unresolved assistant is already in mounted history.
            // Exercise the real recovery caller rather than a synthetic ctx.
            orch.append_external_history_message(current).await;
            assert!(orch
                .run_orphaned_permission(
                    &id,
                    PermissionOutcome::Allow {
                        updated_input: None,
                        permission_updates: Vec::new(),
                        decision_classification: None,
                    },
                )
                .await
                .unwrap());
            assert_eq!(slot.load(Ordering::SeqCst), expected_ended);

            let source = std::fs::read_to_string(&path).unwrap();
            let result = source
                .lines()
                .map(|line| Utf16JsonProjection::parse(line).unwrap())
                .find(|row| row.string_at("/message/content/0/tool_use_id") == Some(id.as_str()))
                .expect("recovery durably writes the real tool result");
            assert_eq!(
                result
                    .at_pointer("/toolUseResult/ended")
                    .and_then(|value| value.as_bool()),
                Some(expected_ended)
            );
        }
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
        ]);
        ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        let r = t.call(source_json("{}"), ctx, tx).await.expect("ok");
        assert_eq!(
            r.data
                .get_property("ended")
                .and_then(|value| value.as_bool()),
            Some(false),
            "a fork can never end the conversation"
        );
        assert!(r
            .data
            .get_property("message")
            .and_then(|value| value.as_str())
            .unwrap()
            .starts_with("You are running as a background fork"));
        assert!(
            !t.end_requested.load(Ordering::SeqCst),
            "fork must NOT raise the slot"
        );
    }
}
