//! Forked-agent runner: full agent loop rooted at the parent's cache-safe
//! prompt prefix.
//!
//! **The multi-turn, tool-using fork lives elsewhere — by design.** The full
//! parent-prefix-rooted, tool-iterating forked subagent loop is implemented and
//! wired via the `Agent` tool's fork path, NOT through this runner:
//! `tool_agent::AgentTool::call` (`subagent_type == "fork"`) → `SubagentSpawner`
//! → `agent::PoolSubagentSpawner::spawn` → `StateMachinePool::allocate` →
//! `agent::runner::run_subagent_loop` (the real `loop` to `end_turn`/`max_turns`).
//! All the claude-code fork parity bits are there: the `FORK_AGENT` builtin with
//! `use_exact_tools`, `lingxi_core::host::fork_subagent::build_forked_messages` for the
//! cache-safe prefix, the verbatim parent system prompt, and the
//! `is_in_fork_child` recursion guard. A forked agent that runs tools therefore
//! goes through `AgentTool`, where that guard and the exact-tools pool apply.
//!
//! **This runner is the SINGLE-TURN summarization helper.** It is intentionally
//! one-shot: when a [`SideQueryClient`] is wired via
//! [`ForkedAgentRunner::with_side_query_client`], [`ForkedAgentRunner::run`]
//! replays the parent's cache-safe prefix
//! (`cache_safe_params.fork_context_messages`) ahead of the fork's own
//! `prompt_messages` so Anthropic's prompt cache hits the shared prefix, then
//! issues ONE stateless LLM call and returns the assistant text + usage. No tool
//! loop runs here — its consumers are summarization-shaped (autocompaction and
//! the post-session memory extraction). When no client is wired, [`run`] returns
//! the legacy `"[forked-agent-stub]"` sentinel.
//!
//! **Cycle note:** `lingxi-sidequery` deliberately does not depend on
//! `lingxi-agent` (the cycle would be `agent → memory → sidequery`). The
//! multi-turn, tool-using fork loop therefore lives in `AgentTool`
//! (`StateMachinePool::run_subagent_loop`), not here; this runner is the
//! single-turn (summarization-shaped) path only. A future *sidequery-level*
//! multi-turn consumer that genuinely cannot reach `AgentTool` would need its
//! own slot-allocation seam wired in at that point.
//!
//! [`run`]: ForkedAgentRunner::run

use crate::cache_safe_params::CacheSafeParams;
use crate::purposes::QuerySource;
use crate::side_query::{SideQueryClient, SideQueryError, SideQueryRequest};
use lingxi_core::types::ConversationMessage;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// Legacy output cap for non-compaction single-turn forks without an override.
/// Compaction inherits the main model's ordinary request budget instead.
const DEFAULT_FORK_MAX_TOKENS: u32 = 20_000;

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
    pub final_text: String,
    /// Tool calls emitted by the one-shot provider response. The runner does
    /// not execute them, but callers such as `/btw` must be able to render an
    /// explicit denial instead of silently turning a tool-only response into
    /// an empty answer.
    pub tool_calls: Vec<serde_json::Value>,
    /// Token / cost usage for COGS attribution.
    pub usage: cost::Usage,
}

/// Forked-agent failure surface.
#[derive(Debug, Clone, Error)]
pub enum ForkError {
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

/// Runs single-turn forked agents (summarization-shaped side queries).
///
/// When a [`SideQueryClient`] is wired via [`Self::with_side_query_client`],
/// [`Self::run`] performs a real single-turn forked call (see the module
/// docs); otherwise it returns the legacy stub sentinel.
pub struct ForkedAgentRunner {
    /// Optional single-turn backend: `(client, model)`. `None` until a caller
    /// opts in via [`Self::with_side_query_client`], preserving the stub path.
    side_query: Option<(Arc<dyn SideQueryClient>, String)>,
    /// Session thinking configuration this runner's forked calls INHERIT
    /// (cc 2.1.198 "Subagents + compaction inherit extended thinking config"
    /// — binary `mXt(r)` threads the session `options.thinkingConfig` into
    /// the summarizer call @216945141). Wired by the composition root for the
    /// COMPACTION runner via [`Self::with_session_thinking`]; `None` (default
    /// — memory extraction, tests) keeps the legacy no-`thinking` wire.
    session_thinking: Option<llm_runtime::model::thinking::ThinkingConfig>,
}

impl Default for ForkedAgentRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ForkedAgentRunner {
    /// Build a runner with no single-turn backend; call
    /// [`Self::with_side_query_client`] to opt into the real single-turn path.
    #[must_use]
    pub fn new() -> Self {
        Self {
            side_query: None,
            session_thinking: None,
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

    /// Inherit the SESSION thinking configuration on this runner's forked
    /// calls (cc 2.1.198): the composition root passes the same session
    /// `ThinkingConfig` the main-loop `ApiService` holds, so a compaction
    /// summary request carries the same `thinking` shape as a main-loop turn.
    #[must_use]
    pub fn with_session_thinking(
        mut self,
        thinking: llm_runtime::model::thinking::ThinkingConfig,
    ) -> Self {
        self.session_thinking = Some(thinking);
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
    /// 4. Issue exactly one [`SideQueryClient::query`] and map its response.
    ///
    /// When no backend is wired the runner returns the legacy
    /// `"[forked-agent-stub]"` sentinel, unchanged.
    ///
    /// The FULL multi-turn, tool-using subagent loop runs through `AgentTool`
    /// (`StateMachinePool::run_subagent_loop`), not this single-turn runner.
    ///
    /// # Errors
    ///
    /// Returns [`ForkError::Api`] when the wired
    /// [`SideQueryClient::query`] call fails.
    pub async fn run(&self, req: ForkedAgentRequest) -> Result<ForkedAgentResult, ForkError> {
        let Some((client, model)) = &self.side_query else {
            // No single-turn backend wired: preserve the legacy stub.
            return Ok(ForkedAgentResult {
                final_text: "[forked-agent-stub]".into(),
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
            });
        };

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

        // Inherit the live parent's model so /model switches affect later forks.
        let model = if cp.tool_use_options.main_loop_model.trim().is_empty() {
            model.clone()
        } else {
            cp.tool_use_options.main_loop_model.clone()
        };
        // cc 2.1.261 PCo supplies no max_tokens override to the compaction
        // fork. The 20k threshold reserve is not a summarization output cap.
        let max_tokens = req.max_output_tokens.unwrap_or_else(|| {
            if req.query_source == QuerySource::Compaction {
                u32::try_from(
                    llm_runtime::model::context_window::default_output_tokens_for_model(&model),
                )
                .unwrap_or(u32::MAX)
            } else {
                DEFAULT_FORK_MAX_TOKENS
            }
        });

        let request = SideQueryRequest {
            model_attempt: None,
            model,
            profile: cp.tool_use_options.model_profile.clone(),
            // Replay the parent's already-rendered system prompt verbatim;
            // `user_context` / `system_context` were inputs the parent used to
            // render it and must NOT be re-applied here (double-rendering would
            // break the byte-identical layout the cache relies on).
            system_prompt: Some(cp.system_prompt.to_string()),
            messages,
            // Claude Code's `runForkedAgent` invokes `query` with an isolated
            // copy of the parent ToolUseContext. That context retains the
            // parent's tools for every fork purpose, including `/btw` and
            // `/recap`; tools are part of the prompt-cache key. Execution is
            // controlled independently by the caller's tool policy and, here,
            // by the absence of a follow-up tool loop.
            tools: cp.tools.clone(),
            tool_choice: None,
            output_format: None,
            max_tokens,
            // Host owns retry policy for forked calls.
            max_retries: 0,
            temperature: None,
            // cc 2.1.198: the forked (compaction) call inherits the session
            // thinking config wired at the composition root; `None` = legacy.
            thinking: self.session_thinking,
            effort: if req.query_source == QuerySource::Compaction
                || matches!(&req.query_source, QuerySource::Custom(source) if source == "side_question")
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

        let resp = client.query(request).await.map_err(ForkError::Api)?;

        Ok(ForkedAgentResult {
            final_text: resp.text.unwrap_or_default(),
            tool_calls: resp.tool_calls,
            usage: resp.usage,
        })
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
                text: Some(self.canned_text.clone()),
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
                tool_calls: vec![serde_json::json!({
                    "id": "toolu_1",
                    "name": "Read",
                    "input": {"file_path": "README.md"}
                })],
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

    fn request_with(
        prefix: Vec<ConversationMessage>,
        prompt: Vec<ConversationMessage>,
        max_output_tokens: Option<u32>,
    ) -> ForkedAgentRequest {
        ForkedAgentRequest {
            prompt_messages: prompt,
            cache_safe_params: CacheSafeParams {
                system_prompt: Arc::from("PARENT SYSTEM PROMPT"),
                user_context: std::collections::HashMap::new(),
                system_context: std::collections::HashMap::new(),
                user_context_message: None,
                tool_use_options: tool_use_options(),
                tools: Vec::new(),
                effort: None,
                fork_context_messages: prefix,
                transcript_path: None,
                generation: 7,
            },
            fork_label: "test-fork".into(),
            query_source: QuerySource::Compaction,
            max_output_tokens,
        }
    }

    #[tokio::test]
    async fn run_without_client_returns_stub() {
        let runner = ForkedAgentRunner::new();
        let req = request_with(vec![user_msg("prefix")], vec![user_msg("prompt")], None);

        let result = runner.run(req).await.expect("stub run succeeds");

        assert_eq!(result.final_text, "[forked-agent-stub]");
        // `cost::Usage` does not implement `PartialEq`; assert on its fields.
        assert_eq!(result.usage.tokens, Usage::default().tokens);
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
            serde_json::json!({
                "name": "Read",
                "description": "Read the file exactly as requested.",
                "input_schema": {
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}},
                    "required": ["file_path"]
                }
            }),
            serde_json::json!({
                "name": "Bash",
                "description": "Run a shell command.",
                "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}},
                "cache_control": {"type": "ephemeral"}
            }),
        ];
        req.cache_safe_params.tools = parent_tools.clone();
        req.cache_safe_params.effort = Some(serde_json::json!("high"));

        let result = runner.run(req).await.expect("wired run succeeds");

        // Response mapping: text -> final_text, usage carried through.
        assert_eq!(result.final_text, "FORKED SUMMARY");
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
        assert_eq!(sent.system_prompt.as_deref(), Some("PARENT SYSTEM PROMPT"));
        assert_eq!(sent.max_tokens, 512); // honored override
        assert_eq!(sent.max_retries, 0);
        assert_eq!(sent.effort, Some(serde_json::json!("high")));
        assert_eq!(
            sent.tools, parent_tools,
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
            .with_session_thinking(llm_runtime::model::thinking::ThinkingConfig::default());
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
                u64::from(sent.max_tokens),
                llm_runtime::model::context_window::default_output_tokens_for_model(expected_model),
                "compaction must inherit the ordinary request budget for {expected_model}"
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
        req.cache_safe_params.tools = vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file.",
            "input_schema": {"type": "object"}
        })];
        req.cache_safe_params.effort = Some(serde_json::json!("high"));
        let parent_tools = req.cache_safe_params.tools.clone();
        let result = runner.run(req).await.expect("run succeeds");

        // Empty text maps to an empty String, not a panic.
        assert_eq!(result.final_text, "");

        let sent = client.seen.lock().unwrap().clone().unwrap();
        assert_eq!(sent.max_tokens, DEFAULT_FORK_MAX_TOKENS);
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

        assert_eq!(result.final_text, "");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0]["name"], "Read");
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
