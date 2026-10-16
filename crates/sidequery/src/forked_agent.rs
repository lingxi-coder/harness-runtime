//! Isolated auxiliary forks rooted in the parent's cache-safe prompt prefix.
//!
//! Summarization-shaped callers issue one model request. Prompt suggestions
//! use Native's 50-turn bound and deny every requested tool, then continue
//! with the explicit denial while retaining ordered text candidates. This
//! runner never executes tools or writes the parent transcript/cache snapshot.
//! Full tool-executing subagents use the `Agent` tool and its runner instead.

use crate::cache_safe_params::CacheSafeParams;
use crate::purposes::QuerySource;
use crate::side_query::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;

/// Legacy output cap for non-compaction single-turn forks without an override.
/// Compaction inherits the main model's ordinary request budget instead.
const DEFAULT_FORK_MAX_TOKENS: u32 = 20_000;

fn is_empty_instruction_projection(message: &ConversationMessage) -> bool {
    matches!(message, ConversationMessage::System { content, subtype, .. }
        if content.string_at("").is_some_and(str::is_empty) && subtype.as_deref() == Some("model_reminder_attachment"))
}

fn task_deliveries_for_messages(
    messages: &[ConversationMessage],
    task_deliveries: &HashMap<MessageId, lingxi_core::host::task_delivery::TaskDeliveryRequest>,
) -> HashMap<MessageId, lingxi_core::host::task_delivery::TaskDeliveryRequest> {
    let retained_ids = messages
        .iter()
        .map(ConversationMessage::id)
        .collect::<HashSet<_>>();
    task_deliveries
        .iter()
        .filter(|(id, _)| retained_ids.contains(id))
        .map(|(id, delivery)| (*id, delivery.clone()))
        .collect()
}

/// Coarse purpose tag for a forked agent. Mirrors [`QuerySource`] for the
/// subset of purposes that legitimately fork (full loops), and is carried
/// inside [`ForkedAgentRequest`] for log/telemetry rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ForkPurpose {
    /// §13.6 autocompactor.
    Compaction,
    /// §6.5 post-session memory extraction.
    SessionMemoryExtraction,
    /// §10.7 supervisor.
    Supervisor,
    /// Prompt suggestion.
    PromptSuggestion,
    /// Post-turn summary.
    PostTurnSummary,
    /// Classifier with rationale.
    ClassifierExplainer,
    /// §9 skill execution.
    SkillExecution,
    /// Caller-labelled purpose.
    Custom(String),
}

/// Input to one forked-agent run.
#[derive(Clone)]
pub struct ForkedAgentRequest {
    /// Prompt messages appended after the cache-safe prefix.
    pub prompt_messages: Vec<ConversationMessage>,
    /// Native operation budget shared across every compaction prefix and
    /// summary fallback request issued by this forked operation.
    pub api_attempts_left: Option<llm_runtime::ApiAttemptsLeft>,
    /// Snapshot of the parent's prompt prefix to replay verbatim.
    pub cache_safe_params: CacheSafeParams,
    /// Free-form label used in logs (`compaction`, `supervisor`, ...).
    pub fork_label: String,
    /// COGS tag.
    pub query_source: QuerySource,
    /// Override output cap; `None` lets the engine pick a default.
    pub max_output_tokens: Option<u32>,
}

/// Decoded outcome of one forked-agent run.
#[derive(Debug, Clone)]
pub struct ForkedAgentResult {
    /// Aggregated final assistant text.
    pub final_text: Utf16JsonProjection,
    /// Native text candidates, in assistant emission order. Prompt suggestion
    /// normalization selects the first candidate that passes its text rules.
    pub candidates: Vec<String>,
    /// HTTP request id of the first assistant, not its provider message id.
    pub generation_request_id: Option<String>,
    /// Tool calls emitted by the one-shot provider response. The runner does
    /// not execute them, but callers such as `/btw` must be able to render an
    /// explicit denial instead of silently turning a tool-only response into
    /// an empty answer.
    pub tool_calls: Vec<Utf16JsonProjection>,
    /// Token / cost usage for COGS attribution.
    pub usage: cost::Usage,
}

/// Forked-agent failure surface.
#[derive(Debug, Clone, Error)]
pub enum ForkError {
    /// No single-turn model backend has been configured on the runner.
    #[error("no forked-agent backend configured")]
    MissingBackend,
    /// The shared [`crate::CacheSafeParamsSlot`] is empty — the parent has
    /// not completed a turn yet.
    #[error("no cache-safe params available")]
    NoCacheSafeParams,
    /// Provider failure, retained for compaction's overflow and media retries.
    #[error(transparent)]
    Api(#[from] SideQueryError),
    /// Internal logic error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Runs isolated auxiliary forks, with explicit no-tool suggestion continuations.
///
/// When a [`SideQueryClient`] is wired via [`Self::with_side_query_client`],
/// [`Self::run`] performs a real single-turn forked call (see the module
/// docs); otherwise it fails with [`ForkError::MissingBackend`].
pub struct ForkedAgentRunner {
    /// Optional single-turn backend: `(client, model)`. `None` until a caller
    /// configures it via [`Self::with_side_query_client`].
    side_query: Option<(Arc<dyn SideQueryClient>, String)>,
    /// Session thinking projection for forked calls. Native Main can retain a
    /// wider overlay than the generic fork API can represent, so keep that
    /// error until a fork is actually requested. `Ok(None)` (default) keeps
    /// the legacy no-`thinking` wire for runners without session inheritance.
    session_thinking:
        Result<Option<llm_runtime::model::thinking::ThinkingConfig>, llm_runtime::LlmError>,
}

impl Default for ForkedAgentRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ForkedAgentRunner {
    /// Build a runner with no single-turn backend; call
    /// [`Self::with_side_query_client`] before running a single-turn request.
    #[must_use]
    pub fn new() -> Self {
        Self {
            side_query: None,
            session_thinking: Ok(None),
        }
    }

    /// Wire a real single-turn forked path backed by `client`, calling `model`.
    ///
    /// `model` is supplied at construction (rather than per-request or scraped
    /// from `CacheSafeParams.system_context`) because neither
    /// [`ForkedAgentRequest`] nor [`CacheSafeParams`] carries a first-class
    /// model string today, and the wiring site (engine / autocompactor) owns
    /// the active model — e.g. `AutocompactConfig::summary_model` or
    /// `OrchestratorConfig::model`.
    #[must_use]
    pub fn with_side_query_client(
        mut self,
        client: Arc<dyn SideQueryClient>,
        model: String,
    ) -> Self {
        self.side_query = Some((client, model));
        self
    }

    /// Whether this runner can issue a real single-turn model request.
    #[must_use]
    pub fn has_side_query_client(&self) -> bool {
        self.side_query.is_some()
    }

    /// Run a stateless Mod completion through the same provider-backed client
    /// as the fork runner. The request carries its own model and prompt; no
    /// parent cache prefix or transcript is read or written.
    pub async fn query_mod_complete(
        &self,
        request: SideQueryRequest,
    ) -> Result<SideQueryResponse, SideQueryError> {
        let Some((client, _)) = &self.side_query else {
            return Err(SideQueryError::InvalidResponse(
                "model.complete client unavailable".into(),
            ));
        };
        client.query(request).await
    }

    /// Run a utility request preserving ordered system blocks and their cache
    /// controls through the provider-backed client, without a parent fork.
    pub async fn query_with_system_blocks(
        &self,
        request: SideQueryRequest,
        system_blocks: Vec<llm_runtime::SystemBlock>,
    ) -> Result<SideQueryResponse, SideQueryError> {
        let Some((client, _)) = &self.side_query else {
            return Err(SideQueryError::InvalidResponse(
                "side-query client unavailable".into(),
            ));
        };
        Ok(client
            .query_fork(
                request,
                crate::side_query::ForkedSideQueryOptions {
                    system_blocks: Some(system_blocks),
                    ..Default::default()
                },
            )
            .await?
            .response)
    }

    /// Inherit the SESSION thinking configuration on this runner's forked
    /// calls (cc 2.1.198): the composition root passes the same session
    /// `ThinkingConfig` the main-loop `ApiService` holds, so a compaction
    /// summary request carries the same `thinking` shape as a main-loop turn.
    #[must_use]
    pub fn with_session_thinking(
        mut self,
        thinking: Result<llm_runtime::model::thinking::ThinkingConfig, llm_runtime::LlmError>,
    ) -> Self {
        // Keep an unrepresentable Native overlay attached to this runner. A
        // host may still use the main request successfully; only the actual
        // generic fork should surface this projection error.
        self.session_thinking = thinking.map(Some);
        self
    }

    /// Run one forked agent and return its final assistant text + usage.
    ///
    /// When a single-turn backend is wired (via
    /// [`Self::with_side_query_client`]) this performs a real, single-turn
    /// forked call:
    ///
    /// 1. Replay the parent's cache-safe prefix
    ///    (`cache_safe_params.fork_context_messages`) *first*, then append the
    ///    fork's own `prompt_messages`, so the shared prefix is byte-identical
    ///    to the parent's and Anthropic's prompt cache hits.
    /// 2. Reuse the parent's already-rendered `system_prompt` verbatim and set
    ///    `skip_system_prompt_prefix` so the client does not prepend its own
    ///    engine prefix (which would break the cache hit).
    /// 3. Preserve the parent's tool schemas for the cache prefix on every
    ///    fork. Claude Code's `runForkedAgent` reuses the parent
    ///    `toolUseContext` regardless of purpose, because tools are part of
    ///    the cache key. Whether a fork may *execute* a tool is a separate
    ///    policy concern: callers such as `/btw` deny every tool and this
    ///    runner never starts a follow-up tool loop.
    /// 4. Issue a model query. Suggestions continue with explicit tool denials;
    ///    every other auxiliary purpose remains one-shot.
    ///
    /// When no backend is wired the runner fails without producing a result.
    ///
    /// The FULL multi-turn, tool-using subagent loop runs through `AgentTool`
    /// (`StateMachinePool::run_subagent_loop`), not this single-turn runner.
    ///
    /// # Errors
    ///
    /// Returns [`ForkError::MissingBackend`] when no backend is configured,
    /// or [`ForkError::Api`] when the wired [`SideQueryClient::query`] fails.
    pub async fn run(&self, req: ForkedAgentRequest) -> Result<ForkedAgentResult, ForkError> {
        let Some((client, model)) = &self.side_query else {
            return Err(ForkError::MissingBackend);
        };
        let session_thinking = self.session_thinking.as_ref().copied().map_err(|error| {
            ForkError::Api(crate::side_query::SideQueryError::Api(error.clone()))
        })?;

        let cp = &req.cache_safe_params;

        // Replay the cache-safe PREFIX first, then the fork's own prompt, so
        // Anthropic's prompt cache hits on the shared, byte-identical prefix.
        let user_context_count = usize::from(cp.user_context_message.is_some());
        let mut messages = Vec::with_capacity(
            user_context_count + cp.fork_context_messages.len() + req.prompt_messages.len(),
        );
        if let Some(message) = &cp.user_context_message {
            messages.push(message.clone());
        }
        messages.extend(cp.fork_context_messages.iter().cloned());
        messages.extend(req.prompt_messages.iter().cloned());
        // Empty announcement projections carry a raw durable cursor, but the
        // native renderer contributes no API message (or model tokens).
        messages.retain(|message| !is_empty_instruction_projection(message));
        let task_deliveries = task_deliveries_for_messages(&messages, &cp.task_deliveries);

        // Inherit the live parent's model so /model switches affect later forks.
        let model = if cp.tool_use_options.main_loop_model.trim().is_empty() {
            model.clone()
        } else {
            cp.tool_use_options.main_loop_model.clone()
        };
        // cc 2.1.261 PCo supplies no max_tokens override to the compaction
        // fork. The 20k threshold reserve is not a summarization output cap.
        let max_tokens = req.max_output_tokens.or_else(|| {
            (!matches!(
                req.query_source,
                QuerySource::Compaction | QuerySource::PromptSuggestion
            ))
            .then_some(DEFAULT_FORK_MAX_TOKENS)
        });

        let mut request = SideQueryRequest {
            model_attempt: None,
            api_attempts_left: req.api_attempts_left.clone(),
            native_prompt_context: None,
            model,
            profile: cp.tool_use_options.model_profile.clone(),
            model_source: cp.model_source.clone(),
            // Replay the parent's already-rendered system prompt verbatim;
            // `user_context` / `system_context` were inputs the parent used to
            // render it and must NOT be re-applied here (double-rendering would
            // break the byte-identical layout the cache relies on).
            system_prompt: Some(cp.system_prompt.as_ref().clone()),
            system_prompt_input: cp.system_prompt_input.clone(),
            enable_prompt_caching: true,
            typed_attachment_ids: cp.typed_attachment_ids.clone(),
            task_deliveries,
            messages,
            // Claude Code's `runForkedAgent` invokes `query` with an isolated
            // copy of the parent ToolUseContext. That context retains the
            // parent's tools for every fork purpose, including `/btw` and
            // `/recap`; tools are part of the prompt-cache key. Execution is
            // controlled independently by the caller's tool policy and, here,
            // by the absence of a follow-up tool loop.
            tools: llm_runtime::convert::to_tool_declarations(cp.tools.clone())
                .map_err(crate::side_query::SideQueryError::Api)
                .map_err(ForkError::Api)?,
            tool_choice: None,
            output_format: None,
            max_tokens,
            // Host owns retry policy for forked calls.
            max_retries: 0,
            temperature: None,
            // cc 2.1.198: the forked (compaction) call inherits the session
            // thinking config wired at the composition root; `None` = legacy.
            thinking: session_thinking,
            effort: if matches!(
                req.query_source,
                QuerySource::Compaction | QuerySource::PromptSuggestion
            ) || matches!(&req.query_source, QuerySource::Custom(source) if source == "side_question" || source == "hook_prompt")
            {
                cp.effort.clone()
            } else {
                None
            },
            stop_sequences: Vec::new(),
            query_source: req.query_source.clone(),
            // The prefix is already baked into `system_prompt`; any extra
            // engine-injected prefix would break the cache hit.
            skip_system_prompt_prefix: true,
        };

        let suggestion = req.query_source == QuerySource::PromptSuggestion;
        // Native MC defaults to 50 model turns. Suggestion's canUseTool always
        // denies; the next model turn receives the exact denial string.
        let max_turns = if suggestion { 50 } else { 1 };
        let mut result = ForkedAgentResult {
            final_text: Utf16JsonProjection::from(""),
            candidates: Vec::new(),
            generation_request_id: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
        };
        let options = crate::side_query::ForkedSideQueryOptions {
            fork_point_message_id: cp.fork_context_messages.last().map(ConversationMessage::id),
            ..Default::default()
        };
        for turn in 0..max_turns {
            let fork = match client.query_fork(request.clone(), options.clone()).await {
                Ok(response) => response,
                // Native I skips an API-error assistant but retains candidates
                // emitted before it; a later failure must not discard them.
                Err(_) if suggestion && turn > 0 => break,
                Err(error) => return Err(ForkError::Api(error)),
            };
            if turn == 0 {
                result.generation_request_id = fork.request_id;
            }
            result.usage.add(&fork.response.usage);
            if !suggestion {
                result.final_text = fork
                    .response
                    .text
                    .unwrap_or_else(|| Utf16JsonProjection::from(""));
                result.candidates = fork.candidates;
                result.tool_calls = fork.response.tool_calls;
                break;
            }
            result.candidates.extend(fork.candidates);
            let denials: Vec<_> = fork
                .assistant_content
                .iter()
                .filter_map(|block| {
                    let ContentBlock::ToolUse {
                        id, provider_id, ..
                    } = block
                    else {
                        return None;
                    };
                    Some(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "No tools needed for suggestion".into(),
                        is_error: Some(true),
                        provider_tool_use_id: provider_id.clone(),
                        content_blocks: None,
                    })
                })
                .collect();
            result.tool_calls.extend(fork.response.tool_calls);
            if denials.is_empty() {
                break;
            }
            request.messages.push(ConversationMessage::Assistant {
                id: MessageId::new(),
                content: fork.assistant_content,
                stop_reason: fork.response.stop_reason,
                resumable: None,
                per_turn_effort: None,
            });
            request.messages.push(ConversationMessage::User {
                id: MessageId::new(),
                content: denials,
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                api_message_override: None,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
            });
        }
        if suggestion {
            result.final_text = result
                .candidates
                .first()
                .cloned()
                .map(Utf16JsonProjection::from)
                .unwrap_or_else(|| Utf16JsonProjection::from(""));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_query::{SideQueryError, SideQueryResponse};
    use async_trait::async_trait;
    use cost::Usage;
    use lingxi_core::types::{ConversationMessage, MessageId};
    use std::sync::Mutex;
    use tool_api::context::ToolUseOptions;

    /// Mock `SideQueryClient`: records the request it was handed and returns a
    /// canned response so tests can assert both the request mapping and the
    /// response mapping.
    struct MockClient {
        seen: Mutex<Option<SideQueryRequest>>,
        canned_text: String,
        canned_usage: Usage,
    }

    #[async_trait]
    impl SideQueryClient for MockClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            *self.seen.lock().unwrap() = Some(request);
            Ok(SideQueryResponse {
                text: Some(Utf16JsonProjection::from(self.canned_text.clone())),
                structured: None,
                tool_calls: Vec::new(),
                usage: self.canned_usage,
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    /// Mock `SideQueryClient` that always fails so typed errors can be checked.
    struct FailingClient {
        error: SideQueryError,
    }

    struct ToolCallClient {
        seen: Mutex<Option<SideQueryRequest>>,
    }

    #[async_trait]
    impl SideQueryClient for ToolCallClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            *self.seen.lock().unwrap() = Some(request);
            Ok(SideQueryResponse {
                text: None,
                structured: None,
                tool_calls: vec![Utf16JsonProjection::object(vec![
                    ("id".into(), "toolu_1".into()),
                    ("name".into(), "Read".into()),
                    (
                        "input".into(),
                        Utf16JsonProjection::parse(r#"{"file_path":"README.md"}"#).unwrap(),
                    ),
                ])
                .unwrap()],
                usage: Usage::default(),
                stop_reason: Some("tool_use".into()),
                retry_count: 0,
            })
        }
    }

    #[async_trait]
    impl SideQueryClient for FailingClient {
        async fn query(
            &self,
            _request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            Err(self.error.clone())
        }
    }

    fn tool_use_options() -> ToolUseOptions {
        ToolUseOptions {
            debug: false,
            verbose: false,
            main_loop_model: "test".into(),
            model_profile: None,
            max_budget_nano_usd: None,
            mcp_clients: vec![],
            is_non_interactive_session: false,
            custom_system_prompt: None,
            append_system_prompt: None,
        }
    }

    fn user_msg(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.into())
    }

    fn source_tool(
        name: &str,
        description: &str,
        input_schema: &str,
    ) -> lingxi_core::host::SourceToolWireDefinition {
        lingxi_core::host::SourceToolWireDefinition {
            name: name.to_owned(),
            description: lingxi_core::types::utf16_json::Utf16JsonProjection::from(description),
            input_schema: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(input_schema)
                .unwrap(),
            strict: false,
            defer_loading: false,
        }
    }

    fn request_with(
        prefix: Vec<ConversationMessage>,
        prompt: Vec<ConversationMessage>,
        max_output_tokens: Option<u32>,
    ) -> ForkedAgentRequest {
        ForkedAgentRequest {
            api_attempts_left: None,
            prompt_messages: prompt,
            cache_safe_params: CacheSafeParams {
                system_prompt: Arc::new(Utf16JsonProjection::from("PARENT SYSTEM PROMPT")),
                system_prompt_input: None,
                user_context: std::collections::HashMap::new(),
                system_context: std::collections::HashMap::new(),
                user_context_message: None,
                tool_use_options: tool_use_options(),
                model_source: None,
                tools: Vec::new(),
                effort: None,
                fork_context_messages: prefix,
                transcript_path: None,
                typed_attachment_ids: std::collections::HashSet::new(),
                task_deliveries: std::collections::HashMap::new(),
                generation: 7,
            },
            fork_label: "test-fork".into(),
            query_source: QuerySource::Compaction,
            max_output_tokens,
        }
    }

    #[tokio::test]
    async fn run_without_backend_returns_typed_failure_without_a_result() {
        for runner in [ForkedAgentRunner::new(), ForkedAgentRunner::default()] {
            assert!(!runner.has_side_query_client());
            for source in [
                QuerySource::Compaction,
                QuerySource::SessionMemoryExtraction,
            ] {
                let mut req =
                    request_with(vec![user_msg("prefix")], vec![user_msg("prompt")], None);
                req.query_source = source;
                let error = runner
                    .run(req)
                    .await
                    .expect_err("missing backend cannot produce body, tool calls, or usage");
                assert!(matches!(&error, ForkError::MissingBackend));
                assert_eq!(error.to_string(), "no forked-agent backend configured");
            }
        }
    }

    #[tokio::test]
    async fn compaction_request_omits_only_empty_announcement_projections() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "SUMMARY".into(),
            canned_usage: Usage::default(),
        });
        let runner = ForkedAgentRunner::new().with_side_query_client(client.clone(), "test".into());
        let placeholder = ConversationMessage::System {
            api_system: None,
            id: MessageId::new(),
            content: String::new().into(),
            subtype: Some("model_reminder_attachment".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let boundary = ConversationMessage::System {
            api_system: None,
            id: MessageId::new(),
            content: String::new().into(),
            subtype: Some("compact_boundary".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let prefix = vec![user_msg("history"), placeholder, boundary.clone()];
        let request = request_with(prefix.clone(), vec![user_msg("summarize")], None);
        runner.run(request).await.unwrap();
        let sent = client.seen.lock().unwrap().clone().unwrap();
        assert_eq!(sent.messages.len(), 3);
        assert!(!sent.messages.iter().any(is_empty_instruction_projection));
        assert_eq!(sent.messages[1], boundary);
        assert_eq!(prefix.len(), 3, "durable input history is not rewritten");
    }

    #[tokio::test]
    async fn forked_call_replays_only_cold_task_deliveries_for_retained_history_rows() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "SUMMARY".into(),
            canned_usage: Usage::default(),
        });
        let runner = ForkedAgentRunner::new().with_side_query_client(client.clone(), "test".into());
        let retained_id = MessageId::new();
        let stale_id = MessageId::new();
        let delivery = Utf16JsonProjection::parse(
            r#"{"taskId":"task-42","status":"completed","summary":"done"}"#,
        )
        .unwrap();
        let stale_delivery = Utf16JsonProjection::parse(r#"{"taskId":"old-task"}"#).unwrap();

        // Exercise the persisted request-history carrier used after cold resume,
        // then transfer its raw values to the cache-safe prefix snapshot.
        let mut restored = lingxi_core::host::NativeRequestHistoryMetadata::default();
        restored
            .task_deliveries
            .insert(retained_id, delivery.clone());
        restored.task_deliveries.insert(stale_id, stale_delivery);
        let restored: lingxi_core::host::NativeRequestHistoryMetadata =
            serde_json::from_value(serde_json::to_value(restored).unwrap()).unwrap();

        let placeholder = ConversationMessage::System {
            api_system: None,
            id: MessageId::new(),
            content: String::new().into(),
            subtype: Some("model_reminder_attachment".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let prefix = vec![
            ConversationMessage::user(retained_id, "delivered task row".into()),
            placeholder,
        ];
        let mut request = request_with(prefix, vec![user_msg("summarize")], None);
        let placement = lingxi_core::host::task_delivery::TaskDeliveryPlacement {
            starts_turn: true,
            in_drain_position: false,
            behind_earlier_messages: false,
            in_human_turn: false,
        };
        let stale_placement = lingxi_core::host::task_delivery::TaskDeliveryPlacement {
            starts_turn: false,
            in_drain_position: false,
            behind_earlier_messages: true,
            in_human_turn: false,
        };
        request.cache_safe_params.task_deliveries = restored
            .task_deliveries
            .into_iter()
            .map(|(id, snapshot)| {
                let placement = if id == retained_id {
                    placement
                } else {
                    stale_placement
                };
                (
                    id,
                    lingxi_core::host::task_delivery::TaskDeliveryRequest {
                        snapshot,
                        placement,
                    },
                )
            })
            .collect();

        runner.run(request).await.expect("fork query succeeds");
        let sent = client.seen.lock().unwrap().clone().expect("client called");
        assert_eq!(sent.messages.len(), 2);
        assert_eq!(sent.task_deliveries.len(), 1);
        let retained = sent
            .task_deliveries
            .get(&retained_id)
            .expect("retained task-delivery metadata");
        assert_eq!(retained.snapshot, delivery);
        assert_eq!(retained.placement, placement);

        let serialized = serde_json::to_string(&sent).unwrap();
        assert!(
            !serialized.contains("task_deliveries"),
            "host task snapshots stay outside the serialized side-query request"
        );
        let decoded: SideQueryRequest = serde_json::from_str(&serialized).unwrap();
        assert!(decoded.task_deliveries.is_empty());
    }

    #[tokio::test]
    async fn compaction_preserves_each_parent_provider_route() {
        for (profile, model) in [
            ("anthropic-profile", "claude-model"),
            ("openai-profile", "openai-model"),
            ("google-profile", "gemini-model"),
            ("custom-profile", "custom-model"),
        ] {
            let client = Arc::new(MockClient {
                seen: Mutex::new(None),
                canned_text: "SUMMARY".into(),
                canned_usage: Usage::default(),
            });
            let runner = ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "startup-model".into());
            let mut req =
                request_with(vec![user_msg("history")], vec![user_msg("summarize")], None);
            req.cache_safe_params.tool_use_options.main_loop_model = model.into();
            req.cache_safe_params.tool_use_options.model_profile = Some(profile.into());
            runner.run(req).await.expect("provider-routed summary");
            let sent = client
                .seen
                .lock()
                .unwrap()
                .clone()
                .expect("summary request");
            assert_eq!(sent.model, model);
            assert_eq!(sent.profile.as_deref(), Some(profile));
        }
    }

    #[tokio::test]
    async fn forked_agent_preserves_exact_parent_model_source() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "SUMMARY".into(),
            canned_usage: Usage::default(),
        });
        let runner =
            ForkedAgentRunner::new().with_side_query_client(client.clone(), "startup-model".into());
        let mut req = request_with(vec![user_msg("history")], vec![user_msg("summarize")], None);
        req.cache_safe_params.model_source =
            Some(Utf16JsonProjection::parse(r#""claude-\ud800""#).unwrap());

        runner.run(req).await.expect("source-bearing fork succeeds");
        let sent = client.seen.lock().unwrap().clone().expect("fork request");
        assert_eq!(
            sent.model_source
                .as_ref()
                .and_then(|source| source.string_units("")),
            Some("claude-".encode_utf16().chain([0xd800]).collect())
        );
    }

    #[tokio::test]
    async fn run_with_client_builds_prefix_first_request_and_maps_result() {
        let mut canned_usage = Usage::default();
        canned_usage.tokens.input = 11;
        canned_usage.tokens.output = 22;

        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "FORKED SUMMARY".into(),
            canned_usage,
        });

        let runner = ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "claude-opus-4-6".into());

        let mut req = request_with(
            vec![user_msg("PREFIX-A"), user_msg("PREFIX-B")],
            vec![user_msg("PROMPT-A")],
            Some(512),
        );
        req.cache_safe_params.tool_use_options.model_profile = Some("parent-profile".into());
        let parent_tools = vec![
            source_tool(
                "Read",
                "Read the file exactly as requested.",
                r#"{"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}"#,
            ),
            source_tool(
                "Bash",
                "Run a shell command.",
                r#"{"type":"object","properties":{"command":{"type":"string"}}}"#,
            ),
        ];
        let expected_tools = llm_runtime::convert::to_tool_declarations(parent_tools.clone())
            .expect("source tool schemas convert");
        req.cache_safe_params.tools = parent_tools.clone();
        req.cache_safe_params.effort = Some(serde_json::json!("high"));

        let result = runner.run(req).await.expect("wired run succeeds");

        // Response mapping: text -> final_text, usage carried through.
        assert_eq!(result.final_text.string_at(""), Some("FORKED SUMMARY"));
        assert_eq!(result.usage.tokens.input, 11);
        assert_eq!(result.usage.tokens.output, 22);

        // Request mapping: assert the exact SideQueryRequest the runner built.
        let sent = client
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("client called once");

        assert_eq!(
            sent.model, "test",
            "the side query must inherit the live parent model"
        );
        assert_eq!(
            sent.profile.as_deref(),
            Some("parent-profile"),
            "the side query must inherit the live parent provider route"
        );
        assert_eq!(
            sent.system_prompt
                .as_ref()
                .and_then(|source| source.string_at("")),
            Some("PARENT SYSTEM PROMPT")
        );
        assert_eq!(sent.max_tokens, Some(512)); // honored override
        assert_eq!(sent.max_retries, 0);
        assert_eq!(sent.effort, Some(serde_json::json!("high")));
        assert_eq!(
            sent.tools, expected_tools,
            "compaction must preserve the parent's tool prefix and order"
        );
        assert!(
            sent.tool_choice.is_none(),
            "the compact text-only prompt does not add tool_choice:none"
        );
        assert!(sent.skip_system_prompt_prefix);
        assert_eq!(sent.query_source, QuerySource::Compaction);

        // Prefix-first ordering: cache-safe prefix, THEN the fork's prompt.
        let order: Vec<String> = sent
            .messages
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        assert_eq!(order, vec!["PREFIX-A", "PREFIX-B", "PROMPT-A"]);
    }

    /// cc 2.1.198 "Subagents + compaction inherit extended thinking config" —
    /// the COMPACTION seam half, runner level: the session `ThinkingConfig`
    /// wired at the composition root rides on every forked (compaction) call;
    /// without the wiring the request stays `thinking: None` (legacy wire).
    #[tokio::test]
    async fn forked_call_carries_the_session_thinking_config() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "SUMMARY".into(),
            canned_usage: Usage::default(),
        });
        let runner = ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "claude-opus-4-6".into())
            .with_session_thinking(Ok(llm_runtime::model::thinking::ThinkingConfig::default()));
        let req = request_with(vec![], vec![user_msg("PROMPT")], Some(512));
        runner.run(req).await.expect("wired run succeeds");
        let sent = client.seen.lock().unwrap().clone().expect("client called");
        assert_eq!(
            sent.thinking,
            Some(llm_runtime::model::thinking::ThinkingConfig::Adaptive),
            "the compaction fork inherits the session thinking config"
        );

        // Unwired runner (memory extraction, legacy) → thinking: None.
        let client2 = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "SUMMARY".into(),
            canned_usage: Usage::default(),
        });
        let runner2 = ForkedAgentRunner::new()
            .with_side_query_client(client2.clone(), "claude-opus-4-6".into());
        let req2 = request_with(vec![], vec![user_msg("PROMPT")], Some(512));
        runner2.run(req2).await.expect("wired run succeeds");
        let sent2 = client2.seen.lock().unwrap().clone().expect("client called");
        assert_eq!(sent2.thinking, None, "unwired runners keep the legacy wire");
    }

    #[tokio::test]
    async fn run_with_client_preserves_query_failure() {
        let client = Arc::new(FailingClient {
            error: SideQueryError::InvalidResponse("boom".into()),
        });
        let runner = ForkedAgentRunner::new().with_side_query_client(client, "m".into());

        let req = request_with(vec![user_msg("prefix")], vec![user_msg("prompt")], None);
        let err = runner
            .run(req)
            .await
            .expect_err("failing client surfaces error");

        match err {
            ForkError::Api(SideQueryError::InvalidResponse(msg)) => {
                assert_eq!(msg, "boom");
            }
            other => panic!("expected original invalid response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_with_client_preserves_compaction_api_error_types() {
        for expected in [
            llm_runtime::LlmError::ContextOverflow { token_gap: 12_345 },
            llm_runtime::LlmError::RequestTooLarge,
            llm_runtime::LlmError::InvalidRequest {
                message: "image exceeds 5 MB maximum".into(),
            },
        ] {
            let client = Arc::new(FailingClient {
                error: SideQueryError::Api(expected.clone()),
            });
            let runner = ForkedAgentRunner::new().with_side_query_client(client, "m".into());
            let err = runner
                .run(request_with(vec![], vec![user_msg("prompt")], None))
                .await
                .expect_err("API failure must retain its retry classification");
            assert_eq!(err.to_string(), expected.to_string());
            match err {
                ForkError::Api(SideQueryError::Api(actual)) => assert_eq!(actual, expected),
                other => panic!("expected original provider error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn compaction_uses_live_parent_model_default_when_output_cap_unset() {
        for parent_model in ["claude-opus-4-6", "claude-3-opus-20240229", ""] {
            let client = Arc::new(MockClient {
                seen: Mutex::new(None),
                canned_text: "summary".into(),
                canned_usage: Usage::default(),
            });
            let runner = ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "claude-sonnet-4-6".into());
            let mut req = request_with(vec![], vec![user_msg("prompt")], None);
            req.cache_safe_params.tool_use_options.main_loop_model = parent_model.into();

            runner.run(req).await.expect("compaction succeeds");

            let sent = client.seen.lock().unwrap().clone().unwrap();
            let expected_model = if parent_model.is_empty() {
                "claude-sonnet-4-6"
            } else {
                parent_model
            };
            assert_eq!(sent.model, expected_model);
            assert_eq!(
                sent.max_tokens, None,
                "compaction must preserve automatic budget for {expected_model}"
            );
        }
    }

    #[tokio::test]
    async fn non_compaction_fork_preserves_parent_tools_and_defaults_max_tokens() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: String::new(),
            canned_usage: Usage::default(),
        });
        let runner = ForkedAgentRunner::new().with_side_query_client(client.clone(), "m".into());

        let mut req = request_with(vec![], vec![user_msg("only-prompt")], None);
        // `/btw` is a tool-denied, one-turn fork, but it still needs the
        // parent schemas in the API request to preserve the shared cache key.
        req.query_source = QuerySource::Custom("side_question".into());
        req.cache_safe_params.tools =
            vec![source_tool("Read", "Read a file.", r#"{"type":"object"}"#)];
        req.cache_safe_params.effort = Some(serde_json::json!("high"));
        let parent_tools =
            llm_runtime::convert::to_tool_declarations(req.cache_safe_params.tools.clone())
                .expect("source tool schemas convert");
        let result = runner.run(req).await.expect("run succeeds");

        // Empty text maps to an empty String, not a panic.
        assert_eq!(result.final_text.string_at(""), Some(""));

        let sent = client.seen.lock().unwrap().clone().unwrap();
        assert_eq!(sent.max_tokens, Some(DEFAULT_FORK_MAX_TOKENS));
        assert_eq!(
            sent.effort,
            Some(serde_json::json!("high")),
            "side questions inherit effort to preserve the parent cache key"
        );
        assert_eq!(
            sent.tools, parent_tools,
            "every fork preserves the parent's tool schemas for the shared cache prefix"
        );
        // No prefix => messages are exactly the fork's prompt.
        let order: Vec<String> = sent
            .messages
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        assert_eq!(order, vec!["only-prompt"]);
    }

    #[tokio::test]
    async fn one_shot_runner_preserves_tool_calls_for_callers_to_deny() {
        let client = Arc::new(ToolCallClient {
            seen: Mutex::new(None),
        });
        let runner =
            ForkedAgentRunner::new().with_side_query_client(client, "claude-sonnet-4-6".into());
        let result = runner
            .run(request_with(vec![], vec![user_msg("question")], Some(128)))
            .await
            .expect("tool-only response is still a decoded result");

        assert_eq!(result.final_text.string_at(""), Some(""));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].string_at("/name"), Some("Read"));
    }

    #[tokio::test]
    async fn transient_user_context_precedes_the_cacheable_prefix() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "answer".into(),
            canned_usage: Usage::default(),
        });
        let runner = ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "claude-sonnet-4-6".into());
        let mut req = request_with(vec![user_msg("prefix")], vec![user_msg("question")], None);
        req.cache_safe_params.user_context_message = Some(ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>context</system-reminder>".into(),
        ));
        runner.run(req).await.expect("context request succeeds");
        let sent = client
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("request captured");
        assert_eq!(
            sent.messages
                .iter()
                .map(ConversationMessage::text_content)
                .collect::<Vec<_>>(),
            vec![
                "<system-reminder>context</system-reminder>",
                "prefix",
                "question"
            ]
        );
    }
}
