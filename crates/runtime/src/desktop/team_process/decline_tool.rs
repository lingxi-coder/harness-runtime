//! The no-reply tool of a task that may stay silent. Either nobody addressed
//! the Bot (it was shown a group Chat message), or a member wrote to it directly
//! in a Bot DM with a message that may need no answer. A call returns at once and the turn ends normally; the task then ends as
//! `declined` and nothing is posted. The durable transcript is the record of
//! the choice, so a continued process reaches the same outcome.
use super::checkpoint::NO_REPLY_TOOL;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tool_api::tool_trait::*;

/// Whether this process saw the Bot choose silence, and how the message
/// reached the Bot.
pub(crate) struct NoReply {
    chosen: AtomicBool,
    addressed: bool,
}
impl NoReply {
    pub(super) fn new(addressed: bool) -> Self {
        Self {
            chosen: AtomicBool::new(false),
            addressed,
        }
    }
    pub(super) fn chosen(&self) -> bool {
        self.chosen.load(Ordering::Acquire)
    }
    /// The member wrote to the Bot directly (a Bot DM).
    pub(super) fn addressed(&self) -> bool {
        self.addressed
    }
}

struct NoReplyTool {
    choice: Arc<NoReply>,
    schema: Json,
}
pub(super) fn register(registry: &mut tool_api::ToolRegistry, choice: Arc<NoReply>) {
    let schema = serde_json::json!({"type":"object","properties":{},"additionalProperties":false});
    registry.register_builtin(Arc::new(NoReplyTool {
        choice,
        schema: generated_builtin_schema(&schema),
    }));
}
#[async_trait::async_trait]
impl Tool for NoReplyTool {
    fn name(&self) -> &str {
        NO_REPLY_TOOL
    }
    fn input_schema(&self) -> &Json {
        &self.schema
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Json) -> bool {
        true
    }
    // Repeating the call changes nothing outside this process, which is what
    // lets a continued task replay it like a read.
    fn is_read_only(&self, _: &Json) -> bool {
        true
    }
    async fn description(&self, _: &Json, _: &DescriptionOptions) -> String {
        "Stay silent: post nothing in the Chat for this message.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Json {
        if self.choice.addressed {
            return "The member wrote this message to you directly. Answer it, unless it plainly needs no answer from you: a thank-you, an acknowledgement or a goodbye that ends the exchange. Only then call this tool and end your turn without writing anything. When in doubt, answer. After this tool is called nothing you write is posted.".into();
        }
        "Nobody asked you directly this time: you were shown the latest Chat message because you take part in this Chat. Decide whether a reply from you is wanted. Reply when the message asks you something, continues an exchange with you, or clearly needs what you are here for. Otherwise call this tool and then end your turn without writing anything: talk between other people, acknowledgements, thanks and messages meant for someone else need no reply from you. When in doubt, stay silent. After this tool is called nothing you write is posted.".into()
    }
    async fn check_permissions(
        &self,
        _: &Json,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Published Team capability: leave a message unanswered".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn call(
        &self,
        _: Json,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.choice.chosen.store(true, Ordering::Release);
        Ok(ToolCallResult {
            data: generated_builtin_result(&serde_json::json!({
                "posted": false,
                "next": "End your turn now without writing anything."
            })),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}
