//! The `StructuredOutput` tool for `--json-schema` structured output.
//!
//! The caller compiles Native's Draft-07 schema before sending the first
//! provider request. This tool validates the model's source projection with that
//! plan, captures only accepted output, and returns the canonical acknowledgement.
//!
//! The existing query driver owns correction retries and the accepted slot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use lingxi_core::host::json_schema_validator::{
    CompiledJsonSchema, JsonSchemaValidation, SchemaWorkerError,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use tool_api::tool_trait::{
    source_text_projection, DescriptionOptions, PromptOptions, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::{Tool, ToolCallResult, ToolError, ToolProgressSender, ToolUseContext};

/// Query-owned retry bookkeeping. Existing history before the first model
/// cycle is a baseline, so resumed/injected old failures do not spend this
/// query's budget. One failed assistant iteration counts once even if it
/// contains several parallel StructuredOutput calls.
pub(crate) struct StructuredOutputRetryState {
    enabled: bool,
    initialized: bool,
    known_calls: HashSet<ToolUseId>,
    current_calls: HashMap<ToolUseId, MessageId>,
    failed_iterations: HashSet<MessageId>,
    failed_calls: HashSet<ToolUseId>,
    failed_attempts: u32,
    last_error: Option<Utf16JsonProjection>,
    succeeded: bool,
    completion_admitted: bool,
    missing_tool_reminded: bool,
    cursor: usize,
    last_message: Option<MessageId>,
}

impl StructuredOutputRetryState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            initialized: false,
            known_calls: HashSet::new(),
            current_calls: HashMap::new(),
            failed_iterations: HashSet::new(),
            failed_calls: HashSet::new(),
            failed_attempts: 0,
            last_error: None,
            succeeded: false,
            completion_admitted: false,
            missing_tool_reminded: false,
            cursor: 0,
            last_message: None,
        }
    }

    pub(crate) fn observe(&mut self, history: &[ConversationMessage]) {
        if !self.enabled {
            return;
        }
        if !self.initialized {
            for message in history {
                for block in message.tool_calls() {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        if name == STRUCTURED_OUTPUT_TOOL_NAME {
                            self.known_calls.insert(id.clone());
                        }
                    }
                }
            }
            self.initialized = true;
            self.remember_tail(history);
            return;
        }
        // Compaction/replay may replace rows. Retained identities make the
        // fallback scan idempotent, while ordinary appends scan only new rows.
        let offset = if self.cursor <= history.len()
            && (self.cursor == 0
                || history.get(self.cursor - 1).map(ConversationMessage::id) == self.last_message)
        {
            self.cursor
        } else {
            0
        };
        let appended = &history[offset..];
        for message in appended {
            for block in message.tool_calls() {
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    if name == STRUCTURED_OUTPUT_TOOL_NAME && self.known_calls.insert(id.clone()) {
                        self.current_calls.insert(id.clone(), message.id());
                    }
                }
            }
        }
        for message in appended {
            let ConversationMessage::User { content, .. } = message else {
                continue;
            };
            for block in content {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = block
                else {
                    continue;
                };
                let Some(iteration) = self.current_calls.get(tool_use_id) else {
                    continue;
                };
                if *is_error != Some(true) {
                    self.succeeded = true;
                    continue;
                }
                if !self.failed_calls.insert(tool_use_id.clone()) {
                    continue;
                }
                if self.failed_iterations.insert(*iteration) {
                    self.failed_attempts = self.failed_attempts.saturating_add(1);
                }
                self.last_error = Some(content.clone());
            }
        }
        self.remember_tail(history);
    }

    fn remember_tail(&mut self, history: &[ConversationMessage]) {
        self.cursor = history.len();
        self.last_message = history.last().map(ConversationMessage::id);
    }

    /// Admit the successful tool's terminal query cycle exactly once. This
    /// cycle completes from the accepted tool result without another API call.
    pub(crate) fn admit_completion(&mut self) -> bool {
        if !self.succeeded || self.completion_admitted {
            return false;
        }
        self.completion_admitted = true;
        true
    }

    pub(crate) fn take_missing_tool_reminder(&mut self) -> bool {
        if !self.enabled || self.succeeded || self.missing_tool_reminded {
            return false;
        }
        self.missing_tool_reminded = true;
        true
    }

    pub(crate) fn exhausted(&self, max_retries: i64) -> Option<crate::OrchestratorError> {
        (self.enabled && self.failed_attempts > 0 && i64::from(self.failed_attempts) >= max_retries)
            .then(|| crate::OrchestratorError::MaxStructuredOutputRetries {
                max_retries,
                last_error: self.last_error.clone(),
            })
    }
}

/// Canonical name of the synthetic structured-output tool.
pub const STRUCTURED_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// Native 2.1.293's single engine reminder when an ordinary response omitted
/// StructuredOutput. It is a meta message inside the same query.
pub(crate) const STRUCTURED_OUTPUT_ENFORCE_REMINDER: &str = "[structured-output-enforce] You MUST call the StructuredOutput tool to complete this request. Call this tool now.";

/// Shared slot the [`StructuredOutputTool`] writes the model's structured result
/// into after validation. The print path reads it after the turn to emit.
pub type StructuredOutputSlot =
    Arc<Mutex<Option<lingxi_core::types::utf16_json::Utf16JsonProjection>>>;

/// The forced `StructuredOutput` tool, backed by the plan compiled before the
/// model request.
pub struct StructuredOutputTool {
    /// The exact user-supplied JSON schema advertised to the model.
    schema: Utf16JsonProjection,
    /// Native Draft-07 plan for this exact schema.
    compiled: Arc<dyn CompiledJsonSchema>,
    /// Where validated model arguments are captured for the caller.
    captured: StructuredOutputSlot,
}

impl StructuredOutputTool {
    /// Build the tool with the plan compiled for `schema` and its capture slot.
    #[must_use]
    pub fn new(
        schema: Utf16JsonProjection,
        compiled: Arc<dyn CompiledJsonSchema>,
        slot: StructuredOutputSlot,
    ) -> Self {
        Self {
            schema,
            compiled,
            captured: slot,
        }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn supports_persistent_chat(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        STRUCTURED_OUTPUT_TOOL_NAME
    }

    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("return the final response as structured JSON".into())
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }

    fn input_validation_schema(&self) -> &Utf16JsonProjection {
        static BASE: OnceLock<Utf16JsonProjection> = OnceLock::new();
        BASE.get_or_init(|| {
            Utf16JsonProjection::parse(r#"{"type":"object"}"#)
                .expect("static StructuredOutput parser schema is valid")
        })
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::SourceNative,
            exempt_top_level_keys: Some(HashSet::new()),
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // Claude sets maxResultSizeChars to 100,000 for this tool.
        100_000
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn result_ends_turn(&self, _result: &ToolCallResult) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "structured output".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Return structured output in the requested format".to_string()
    }

    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        "Use this tool to return your final response in the requested structured format. You MUST call this tool exactly once at the end of your response to provide the structured output.".to_string().into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        match self
            .compiled
            .validate(&input)
            .await
            .map_err(worker_failure)?
        {
            JsonSchemaValidation::Valid => {
                if let Ok(mut slot) = self.captured.lock() {
                    *slot = Some(input);
                }
            }
            JsonSchemaValidation::Invalid { detail, .. } => {
                return Err(ToolError::InvalidInput(detail));
            }
        }

        Ok(ToolCallResult::from_data(source_text_projection(
            "Structured output provided successfully",
        )))
    }
}

fn worker_failure(error: SchemaWorkerError) -> ToolError {
    ToolError::Internal(format!("Structured output validator failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::json_schema_validator::SchemaWorkerError;
    use std::sync::Mutex as StdMutex;

    struct FixedPlan {
        result: JsonSchemaValidation,
        seen: Arc<StdMutex<Option<Utf16JsonProjection>>>,
    }

    #[async_trait]
    impl CompiledJsonSchema for FixedPlan {
        async fn validate(
            &self,
            input: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            *self.seen.lock().unwrap() = Some(input.clone());
            Ok(self.result.clone())
        }
    }

    fn plan(
        result: JsonSchemaValidation,
    ) -> (
        Arc<dyn CompiledJsonSchema>,
        Arc<StdMutex<Option<Utf16JsonProjection>>>,
    ) {
        let seen = Arc::new(StdMutex::new(None));
        (
            Arc::new(FixedPlan {
                result,
                seen: seen.clone(),
            }),
            seen,
        )
    }

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("valid source JSON fixture")
    }

    #[tokio::test]
    async fn successful_call_validates_and_captures_the_exact_source_projection() {
        let schema = parse(r#"{"type":"object"}"#);
        let input = parse(r#"{"answer":"\ud800"}"#);
        let (compiled, seen) = plan(JsonSchemaValidation::Valid);
        let slot = Arc::new(Mutex::new(None));
        let tool = StructuredOutputTool::new(schema, compiled, slot.clone());
        let (tx, _) = tool_api::progress::progress_channel();
        let result = tool
            .call(input.clone(), ToolUseContext::model_seed("test".into()), tx)
            .await
            .expect("valid output is accepted");

        assert_eq!(seen.lock().unwrap().as_ref(), Some(&input));
        assert_eq!(slot.lock().unwrap().as_ref(), Some(&input));
        assert_eq!(
            result.data.string_at(""),
            Some("Structured output provided successfully")
        );
        assert!(tool.result_ends_turn(&result));
    }

    #[tokio::test]
    async fn invalid_call_returns_exact_validator_detail_without_replacing_capture() {
        let schema = parse(r#"{"type":"object"}"#);
        let previous = parse(r#"{"answer":42}"#);
        let detail = parse(r#""invalid \ud800 output""#);
        let (compiled, _) = plan(JsonSchemaValidation::Invalid {
            detail: detail.clone(),
            keywords: vec!["required".into()],
        });
        let slot = Arc::new(Mutex::new(Some(previous.clone())));
        let tool = StructuredOutputTool::new(schema, compiled, slot.clone());
        let (tx, _) = tool_api::progress::progress_channel();
        let error = tool
            .call(
                parse(r#"{}"#),
                ToolUseContext::model_seed("test".into()),
                tx,
            )
            .await
            .unwrap_err();

        assert_eq!(error.model_facing_message(), detail);
        assert_eq!(slot.lock().unwrap().as_ref(), Some(&previous));
    }
}

#[cfg(test)]
mod query_retry_tests {
    use super::*;

    fn failed_iteration(error: &str) -> Vec<ConversationMessage> {
        let tool_id = ToolUseId::new();
        let assistant = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: tool_id.clone(),
                name: STRUCTURED_OUTPUT_TOOL_NAME.into(),
                input: Utf16JsonProjection::from_serde_value(&serde_json::json!({"answer":42}))
                    .expect("fixture tool input is valid JSON"),
                provider_id: None,
            }],
            stop_reason: Some("tool_use".into()),
            resumable: None,
        };
        let mut result = ConversationMessage::user(MessageId::new(), String::new());
        let ConversationMessage::User { content, .. } = &mut result else {
            unreachable!()
        };
        *content = vec![ContentBlock::ToolResult {
            tool_use_id: tool_id,
            content: Utf16JsonProjection::from(error),
            is_error: Some(true),
            provider_tool_use_id: None,
            content_blocks: None,
        }];
        vec![assistant, result]
    }

    #[test]
    fn failed_structured_iterations_share_one_query_and_keep_the_native_last_error() {
        let mut history = failed_iteration("prior query error");
        history.push(ConversationMessage::user(
            MessageId::new(),
            "original prompt".into(),
        ));
        let mut state = StructuredOutputRetryState::new(true);
        state.observe(&history);
        assert!(
            state.exhausted(2).is_none(),
            "prior transcript must not spend this query budget"
        );
        history.extend(failed_iteration("first schema error"));
        state.observe(&history);
        assert!(state.exhausted(2).is_none());
        let last = "Output does not match required schema: /answer: must be string";
        history.extend(failed_iteration(last));
        state.observe(&history);
        assert_eq!(
            state.exhausted(2).unwrap().to_string(),
            format!(
                "Failed to provide valid structured output after 2 attempts — last StructuredOutput error: {last}"
            )
        );
        state.observe(&history);
        assert_eq!(
            state.failed_attempts, 2,
            "re-reading history cannot double count attempts"
        );
        let mut next_query = StructuredOutputRetryState::new(true);
        next_query.observe(&history);
        assert!(next_query.exhausted(2).is_none());
    }

    #[test]
    fn compaction_or_replay_retains_attempt_identity_and_latest_error() {
        let mut state = StructuredOutputRetryState::new(true);
        state.observe(&[]);
        let old = failed_iteration("first");
        let latest = failed_iteration("latest");
        let mut history = old.clone();
        history.extend(latest);
        state.observe(&history);
        assert_eq!(state.failed_attempts, 2);
        state.observe(&old);
        assert_eq!(state.failed_attempts, 2);
        assert_eq!(
            state
                .last_error
                .as_ref()
                .and_then(|error| error.string_at("")),
            Some("latest")
        );
        history = failed_iteration("after compact");
        state.observe(&history);
        assert_eq!(state.failed_attempts, 3);
        assert_eq!(
            state
                .last_error
                .as_ref()
                .and_then(|error| error.string_at("")),
            Some("after compact")
        );
    }

    #[test]
    fn retry_policy_is_inert_without_the_structured_output_tool() {
        let mut state = StructuredOutputRetryState::new(false);
        state.observe(&failed_iteration("unrelated transcript"));
        assert!(state.exhausted(0).is_none());
    }

    #[test]
    fn successful_tool_result_admits_one_terminal_cycle_for_the_current_query() {
        let mut successful = failed_iteration("Structured output captured successfully");
        let ConversationMessage::User { content, .. } = &mut successful[1] else {
            unreachable!()
        };
        let ContentBlock::ToolResult { is_error, .. } = &mut content[0] else {
            unreachable!()
        };
        *is_error = Some(false);
        let mut state = StructuredOutputRetryState::new(true);
        state.observe(&successful);
        assert!(
            !state.admit_completion(),
            "resumed successful output is a baseline"
        );
        successful.extend(failed_iteration("invalid output"));
        let mut accepted = failed_iteration("Structured output captured successfully");
        let ConversationMessage::User { content, .. } = &mut accepted[1] else {
            unreachable!()
        };
        let ContentBlock::ToolResult { is_error, .. } = &mut content[0] else {
            unreachable!()
        };
        *is_error = None;
        successful.extend(accepted);
        state.observe(&successful);
        assert!(state.admit_completion());
        assert!(!state.admit_completion());
        assert_eq!(state.failed_attempts, 1);
        state.observe(&successful);
        assert!(
            !state.admit_completion(),
            "replay cannot readmit a terminal cycle"
        );
    }
}
