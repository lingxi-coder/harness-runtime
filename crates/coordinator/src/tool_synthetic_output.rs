//! `SyntheticOutputTool` — coordinator-only port of the TS `StructuredOutput`
//! tool (`src/tools/SyntheticOutputTool/SyntheticOutputTool.ts`,
//! `SYNTHETIC_OUTPUT_TOOL_NAME = 'StructuredOutput'`).
//!
//! 1:1 with the TS contract: the model calls this exactly once at the end of a
//! worker's response to return its final structured payload. The TS `call()`
//! returns `{ data: "Structured output provided successfully",
//! structured_output: <input> }` verbatim.
//!
//! The coordinator-side adaptation: in coordinator mode this is the channel by
//! which a worker's synthetic/structured output is surfaced back into the
//! team. We hold an [`Arc<TeamRegistry>`](crate::team_registry::TeamRegistry)
//! and, when the caller identifies the originating worker by `agent_id`, we
//! inject the serialized payload into that worker's mailbox as a
//! [`TeammateMessage`] (so it appears in the coordinator transcript / mailbox).
//! The model-facing return value is unchanged from the TS shape.
//!
//! Wiring note: this file is declared as a module by a separate wire step
//! (`internal_tools.rs` constructs `SyntheticOutputTool::new(team)` and adds it
//! to `coordinator_internal_tools`). It is self-consistent against
//! `crate::team_registry`, `crate::mailbox`, and `tool_api`.

use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use async_trait::async_trait;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use uuid::Uuid;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};

use crate::mailbox::{MessageSender, TeammateMessage};
use crate::team_registry::TeamRegistry;

/// Exact TS tool name (`SYNTHETIC_OUTPUT_TOOL_NAME = 'StructuredOutput'`).
pub const SYNTHETIC_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// Model-facing `data` string returned on success (1:1 with the TS tool).
pub const STRUCTURED_OUTPUT_SUCCESS_DATA: &str = "Structured output provided successfully";

/// TS `maxResultSizeChars: 100_000`.
const MAX_RESULT_SIZE_CHARS: usize = 100_000;

/// Backing store for the cached input schema (built once, on first access).
///
/// `std::sync::OnceLock` is used instead of `once_cell::sync::Lazy` so the
/// coordinator crate does not take a new dependency on `once_cell` (mirrors the
/// sibling coordinator tools `tool_team_create.rs` / `tool_send_message.rs`).
static SYNTHETIC_OUTPUT_SCHEMA: OnceLock<Value> = OnceLock::new();

/// Base input schema. The TS base tool uses `z.object({}).passthrough()` —
/// i.e. it accepts any object and the *real* per-call schema is supplied
/// dynamically by `createSyntheticOutputTool(jsonSchema)`. The coordinator port
/// keeps the permissive base shape and threads through a `structured_output`
/// payload plus an optional `agent_id` identifying the originating worker.
fn synthetic_output_schema() -> &'static Value {
    SYNTHETIC_OUTPUT_SCHEMA.get_or_init(|| {
        json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Optional worker agent id (UUID) whose output this is; the payload is injected into that worker's mailbox."
                },
                "structured_output": {
                    "description": "The structured output payload to return. If omitted, the whole input object is treated as the payload."
                }
            }
        })
    })
}

/// Coordinator-only `StructuredOutput` tool.
///
/// Holds the shared [`TeamRegistry`] so it can route an injected
/// [`TeammateMessage`] into the originating worker's mailbox. The input schema
/// is cached in the `SYNTHETIC_OUTPUT_SCHEMA` `OnceLock`.
pub struct SyntheticOutputTool {
    team: Arc<TeamRegistry>,
}

impl SyntheticOutputTool {
    /// Construct the tool wired to the shared coordinator [`TeamRegistry`].
    #[must_use]
    pub fn new(team: Arc<TeamRegistry>) -> Self {
        Self { team }
    }

    /// Extract the structured payload from `input`.
    ///
    /// Mirrors the TS tool, which returns the entire input object as the
    /// structured output. If the caller nested the payload under an explicit
    /// `structured_output` key we surface that; otherwise the whole input
    /// object is the payload (sans the coordinator routing-only `agent_id`).
    fn extract_payload(input: &Value) -> Value {
        if let Some(payload) = input.get("structured_output") {
            return payload.clone();
        }
        match input {
            Value::Object(map) => {
                let mut cleaned = map.clone();
                cleaned.remove("agent_id");
                Value::Object(cleaned)
            }
            other => other.clone(),
        }
    }
}

#[async_trait]
impl Tool for SyntheticOutputTool {
    fn name(&self) -> &str {
        SYNTHETIC_OUTPUT_TOOL_NAME
    }

    fn input_schema(&self) -> &Value {
        synthetic_output_schema()
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // TS: once created, always enabled (creation is gated upstream by
        // `isSyntheticOutputToolEnabled({ isNonInteractiveSession })`).
        true
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_RESULT_SIZE_CHARS
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        // TS: `isConcurrencySafe() => true`.
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        // TS: `isReadOnly() => true`.
        true
    }

    fn is_destructive(&self, _: &Value) -> bool {
        false
    }

    fn is_open_world(&self, _: &Value) -> bool {
        // TS: `isOpenWorld() => false`.
        false
    }

    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        // TS: `checkPermissions` always allows — it just returns data.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "StructuredOutput just returns the provided data".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    fn search_hint(&self) -> Option<&str> {
        // Binary StructuredOutput: `searchHint:"return the final response as
        // structured JSON"`.
        Some("return the final response as structured JSON")
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        // TS: `description()` => 'Return structured output in the requested format'.
        "Return structured output in the requested format".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        // TS: `prompt()` byte-for-byte.
        "Use this tool to return your final response in the requested structured format. You MUST \
         call this tool exactly once at the end of your response to provide the structured output."
            .into()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // The TS base tool accepts any object. Reject non-object input so we
        // map a malformed call to the trait's error variant rather than
        // silently echoing a scalar.
        if !input.is_object() {
            return Err(ToolError::InvalidInput(
                "StructuredOutput: input must be a JSON object".into(),
            ));
        }

        let payload = Self::extract_payload(&input);

        // Coordinator adaptation: if the caller names the originating worker,
        // inject the serialized payload into that worker's mailbox so the
        // synthetic output surfaces in the coordinator transcript.
        if let Some(agent_id_str) = input.get("agent_id").and_then(Value::as_str) {
            let uuid = Uuid::parse_str(agent_id_str).map_err(|_| {
                ToolError::InvalidInput(format!(
                    "StructuredOutput: 'agent_id' is not a valid UUID: {agent_id_str}"
                ))
            })?;
            let worker_id = protocol::AgentId::from_uuid(uuid);

            // Serialize the structured payload as the message content.
            let content = serde_json::to_string(&payload).map_err(|e| {
                ToolError::Internal(format!(
                    "StructuredOutput: failed to serialize payload: {e}"
                ))
            })?;

            let msg = TeammateMessage {
                from: MessageSender::Teammate(worker_id),
                from_name: self
                    .team
                    .find_by_agent_id(&worker_id)
                    .await
                    .map_or_else(|| worker_id.to_string(), |worker| worker.name),
                content,
                summary: None,
                message_id: tool_api::util::ids::ulid_or_uuid(),
                timestamp: SystemTime::now(),
                request_id: None,
            };

            self.team
                .mailbox_router
                .route(&worker_id, msg)
                .await
                .map_err(|e| match e {
                    crate::mailbox::MailboxError::NotFound(id) => ToolError::InvalidInput(format!(
                        "StructuredOutput: unknown worker agent id: {id}"
                    )),
                    other => ToolError::Internal(format!("StructuredOutput: {other}")),
                })?;
        }

        // Model-facing return value is identical to the TS tool.
        Ok(ToolCallResult {
            data: json!({
                "data": STRUCTURED_OUTPUT_SUCCESS_DATA,
                "structured_output": payload,
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::AgentId;
    use tool_api::context::ToolUseOptions;

    /// Build a minimal [`ToolUseContext`] without depending on the optional
    /// `tool-api/test-support` feature (not enabled for the coordinator crate).
    fn fresh_ctx() -> ToolUseContext {
        ToolUseContext {
            options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: "test".into(),
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: vec![],
                is_non_interactive_session: false,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            messages: vec![],
            tool_use_id: None,
            assistant_message_id: None,
            agent_id: None,
            agent_name: None,
            observer: None,
            observer_pairings: None,
            team_name: None,
            origin_session_id: None,
            tool_execution_policy: platform_api::tool_invoker::ToolExecutionPolicy::Ordinary,
            content_replacement_state: None,
            session: None,
            subagent_registry: None,
            cancel: None,
            fork_parent_system_prompt: None,
            cwd: None,
            depth: 0,
            file_history: None,
        }
    }

    fn fresh_tx() -> ToolProgressSender {
        let (tx, _rx) = tool_api::progress::progress_channel();
        tx
    }

    fn registry() -> Arc<TeamRegistry> {
        Arc::new(TeamRegistry::new(AgentId::new()))
    }

    #[test]
    fn tool_name_matches_ts_constant() {
        assert_eq!(SYNTHETIC_OUTPUT_TOOL_NAME, "StructuredOutput");
        let tool = SyntheticOutputTool::new(registry());
        assert_eq!(tool.name(), "StructuredOutput");
    }

    #[tokio::test]
    async fn returns_input_verbatim_when_no_agent_id() {
        let tool = SyntheticOutputTool::new(registry());
        let out = tool
            .call(
                json!({ "bugs": ["a", "b"], "count": 2 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("call should succeed");
        assert_eq!(out.data["data"], json!(STRUCTURED_OUTPUT_SUCCESS_DATA));
        assert_eq!(
            out.data["structured_output"],
            json!({ "bugs": ["a", "b"], "count": 2 })
        );
    }

    #[tokio::test]
    async fn explicit_structured_output_key_is_surfaced() {
        let tool = SyntheticOutputTool::new(registry());
        let out = tool
            .call(
                json!({ "structured_output": { "result": "ok" } }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("call should succeed");
        assert_eq!(out.data["structured_output"], json!({ "result": "ok" }));
    }

    #[tokio::test]
    async fn injects_output_into_named_worker_mailbox() {
        use crate::mailbox::TeammateMailbox;

        let team = registry();
        // Register a controllable mailbox so we can drain it directly and
        // assert the synthetic output landed (the router owns the worker
        // mailboxes it creates in `spawn_worker`, so we register our own).
        let worker_id = AgentId::new();
        let mailbox = Arc::new(TeammateMailbox::new(worker_id));
        team.mailbox_router
            .register(worker_id, mailbox.clone())
            .await;

        let tool = SyntheticOutputTool::new(team.clone());
        let out = tool
            .call(
                json!({
                    "agent_id": worker_id.as_uuid().to_string(),
                    "structured_output": { "summary": "done" }
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("call should succeed");

        // Model-facing payload excludes the routing-only agent_id.
        assert_eq!(out.data["data"], json!(STRUCTURED_OUTPUT_SUCCESS_DATA));
        assert_eq!(out.data["structured_output"], json!({ "summary": "done" }));

        // Effect: the worker's mailbox now holds exactly the injected output.
        let delivered = mailbox.drain();
        assert_eq!(
            delivered.len(),
            1,
            "exactly one synthetic message delivered"
        );
        assert!(matches!(delivered[0].from, MessageSender::Teammate(id) if id == worker_id));
        assert_eq!(delivered[0].content, r#"{"summary":"done"}"#);
    }

    #[tokio::test]
    async fn rejects_invalid_agent_id() {
        let tool = SyntheticOutputTool::new(registry());
        let err = tool
            .call(
                json!({ "agent_id": "not-a-uuid", "structured_output": {} }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("invalid agent_id must fail");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(format!("{err}").contains("not a valid UUID"));
    }

    #[tokio::test]
    async fn unknown_worker_is_invalid_input() {
        let tool = SyntheticOutputTool::new(registry());
        let orphan = AgentId::new();
        let err = tool
            .call(
                json!({
                    "agent_id": orphan.as_uuid().to_string(),
                    "structured_output": {}
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("unknown worker must fail");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(format!("{err}").contains("unknown worker agent id"));
    }

    #[tokio::test]
    async fn rejects_non_object_input() {
        let tool = SyntheticOutputTool::new(registry());
        let err = tool
            .call(json!("scalar"), fresh_ctx(), fresh_tx())
            .await
            .expect_err("scalar input must fail");
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
