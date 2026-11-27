//! Auxiliary, classifier and structured-output requests.

use crate::dispatch_header::DispatchHeaderState;
use crate::model::retry::{
    resolve_retry_control_with_settings, DriveStep, ResolveRetryEnv, RetryControl, RetryState,
};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::service::anthropic::prompt::native_prompt_attribution;
use crate::service::complete::NonStreamDrivePolicy;
use crate::service::request::{
    custom_system_prompt, prompt_cache_query_source, side_query_system_prompt,
};
use crate::service::retry::reasoning_budget;
use crate::service::{ApiService, MOD_REQUEST_EFFORT};
use crate::{HistoryEvent, HistoryResponse, LlmError, LlmRequest};
use futures::stream::BoxStream;
use lingxi_core::types::ConversationMessage;

impl ApiService {
    /// Execute a canonical side-query request through the same host hooks and
    /// retry driver as the high-level side-query entry points.
    pub async fn execute_side_query_request(
        &self,
        mut request: LlmRequest,
    ) -> Result<HistoryResponse, LlmError> {
        request.stream = false;
        request.execution.isolated_query = true;
        request.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        let mut control = resolve_retry_control_with_settings(
            &request.input.model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        if let Some(max) = request.execution.side_query_max_retries {
            control.max_retries = control.max_retries.min(max);
        }
        self.drive_non_stream(request, control, DispatchHeaderState::AUXILIARY)
            .await
    }

    /// Classifier side queries own a retry budget independent of main turns.
    pub async fn execute_classifier_request(
        &self,
        mut request: LlmRequest,
        max_retries: u32,
    ) -> Result<HistoryResponse, LlmError> {
        request.stream = false;
        request.execution.isolated_query = true;
        request.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        let mut control = resolve_retry_control_with_settings(
            &request.input.model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        control.max_retries = max_retries;
        self.drive_non_stream(request, control, DispatchHeaderState::AUXILIARY)
            .await
    }

    /// Classifier streams are accumulated privately, so a failed body may be
    /// replayed without exposing partial output. Headers and bodies share one
    /// retry ledger; reopening a stream never replenishes the query's budget.
    pub async fn execute_classifier_stream_request(
        &self,
        mut request: LlmRequest,
        max_retries: u32,
    ) -> Result<HistoryResponse, LlmError> {
        request.execution.isolated_query = true;
        request.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        crate::anthropic_extra_body::capture(&mut request)?;
        let mut control = resolve_retry_control_with_settings(
            &request.input.model,
            None,
            false,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        control.max_retries = max_retries;
        let scope = ModelCallRetryScope::default().with_private_output();
        let mut state = RetryState::default();
        let control = scope.configure(&control, &mut state);
        let thinking_budget = reasoning_budget(request.input.thinking.as_ref());
        scope
            .run(async {
                loop {
                    // The stream driver has already exhausted or declined
                    // header retries, including x-should-retry:false. Only a
                    // body failure needs the classifier's outer replay loop.
                    let stream = Box::pin(self.stream_request(request.clone())).await?;
                    let error = match crate::stream_accumulator::accumulate_stream_salvaging(stream)
                        .await
                    {
                        Ok(response) => return Ok(response),
                        Err((_, error)) => error,
                    };
                    if !error.allows_automatic_replay() {
                        return Err(error);
                    }
                    match scope.next_step(&mut state, &control, &error, thinking_budget, None) {
                        DriveStep::RetryAfter(delay) => {
                            self.report_and_sleep_retry(&error, delay, &state, &control)
                                .await;
                        }
                        _ => return Err(error),
                    }
                }
            })
            .await
    }

    /// Execute Native model validation through the shared provider driver with
    /// a request-local Native repair policy.
    ///
    /// `request` supplies the selected route/profile plus the model-validation body
    /// (including its exact `model_source`, `Hi` user message, one-token cap, and
    /// ephemeral message breakpoint). The caller owns Native's outer 15-second
    /// Promise race. This method must not add a deadline or cancel the request: a
    /// late probe is allowed to finish and update the caller's validation cache.
    ///
    /// The request uses the shared route, credential, SDK codec, and transport.
    /// Ordinary retries, timeout retries, model fallback, inherited effort and
    /// connection failover are disabled by `ModelValidation`. The shared driver
    /// keeps only Native request-specific repairs: context-1m refusal,
    /// dispatch-header fallback, captured OAuth revocation, and a Foundry
    /// capability refusal when the cached observation changes the request body.
    pub async fn execute_model_validation_request(
        &self,
        mut request: LlmRequest,
    ) -> Result<HistoryResponse, LlmError> {
        if request.execution.model_attempt.is_some() {
            return Err(crate::model_attempt::missing_hooks_error());
        }

        request.stream = false;
        // This is an isolated utility query, not a turn. Clear session-derived
        // prompt/effort/identity sidecars while preserving the SDK ChatRequest
        // body supplied by the caller.
        request.execution = Default::default();
        request.execution.query_source = Some("model_validation".to_owned());
        request.execution.anthropic_request_kind =
            lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        // Validation owns its minimal caller-supplied body. Pin an empty
        // capture so the common driver cannot inherit session environment.
        request.execution.anthropic_extra_body = Some(
            crate::anthropic_extra_body::CapturedAnthropicExtraBody::new(
                None,
                request.execution.anthropic_request_kind,
            )?,
        );

        let mut retry_control = RetryControl {
            primary_model: request.input.model.clone(),
            ..RetryControl::default()
        };
        retry_control.max_retries = 0;
        retry_control.fallback_model = None;
        retry_control.allow_fallback = false;
        retry_control.watchdog = false;
        self.drive_non_stream_seeded_with_chain(
            request,
            retry_control,
            0,
            &[],
            DispatchHeaderState::AUXILIARY,
            NonStreamDrivePolicy::ModelValidation,
        )
        .await
    }

    pub(super) fn apply_side_query_thinking(
        &self,
        req: &mut LlmRequest,
        model: &str,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
    ) {
        use crate::model::thinking::{model_sends_temperature, session_thinking_active};

        let protocol = req.execution.input_protocol;
        let has_thinking = thinking.is_some_and(session_thinking_active);
        req.execution.automatic_thinking_budget = matches!(
            thinking,
            Some(crate::model::thinking::ThinkingConfig::Adaptive)
        );
        req.execution.side_thinking_disabled = matches!(
            thinking,
            Some(crate::model::thinking::ThinkingConfig::Disabled)
        );
        req.set_reasoning(thinking.and_then(|thinking| {
            crate::model::thinking::reasoning_for_route(
                thinking,
                model,
                if req.execution.automatic_thinking_budget {
                    None
                } else {
                    req.input.max_tokens
                },
                protocol,
            )
        }));
        if req.execution.side_thinking_disabled {
            // `reasoning_for_request(Disabled)` intentionally returns None
            // for non-Claude models. Keep the explicit off intent in the SDK
            // request anyway: codecs use it to distinguish “thinking is off”
            // from “the caller did not specify a thinking mode”. In
            // particular, DeepSeek rejects forced tool choice while thinking
            // is on, but accepts it when the request explicitly disables
            // thinking.
            req.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
                mode: Some(lingxi_llm_client::protocol::ThinkingMode::Disabled),
                ..Default::default()
            });
        }
        req.input.temperature = temperature.or_else(|| {
            if thinking.is_some()
                && !has_thinking
                && protocol.is_some_and(crate::ProtocolFamily::is_anthropic_family)
                && model_sends_temperature(model)
                && !matches!(
                    thinking,
                    Some(crate::model::thinking::ThinkingConfig::Automatic)
                )
            {
                Some(1.0)
            } else {
                None
            }
        });
    }

    /// Build the canonical non-strict side-query request used by both
    /// estimation and the live Session dispatch path.
    #[allow(clippy::too_many_arguments)]
    pub fn build_side_query_request_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        system_input: Option<&crate::SystemPromptInput>,
        skip_global_cache_for_system_prompt: bool,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        effort: Option<serde_json::Value>,
        temperature: Option<f32>,
        query_source: Option<&str>,
        typed_attachment_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
        task_deliveries: std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
    ) -> Result<LlmRequest, LlmError> {
        let system = side_query_system_prompt(system, system_input)?;
        let side_query_attribution = self
            .anthropic
            .native_system_prefix
            .as_ref()
            .map(|_| native_prompt_attribution(&messages, self.ua.workload.clone()));
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request_with_typed_attachment_ids(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                false,
                max_tokens,
                skip_global_cache_for_system_prompt,
                prompt_cache_query_source(query_source),
                &typed_attachment_ids,
                &task_deliveries,
            )
        })?;
        if req
            .execution
            .input_protocol
            .as_ref()
            .is_some_and(Self::is_anthropic_family_protocol)
        {
            if let (Some(prefix), Some(attribution), Some(prompt_cache)) = (
                self.anthropic.native_system_prefix.as_ref(),
                side_query_attribution,
                req.execution.prompt_cache.as_mut(),
            ) {
                // Native bze carries Fmo attribution on Anthropic-family side
                // queries even when the route is hosted. Main-request route
                // admission remains unchanged, and identity stays opt-in in
                // the side-query execution context.
                if prompt_cache.native_system_prefix.is_none() {
                    prompt_cache.native_system_prefix = Some((prefix.clone(), attribution));
                }
            }
        }
        // A main turn's Mod effort is task-local. Side queries assembled in
        // that same task keep their own explicit/default effort instead.
        if MOD_REQUEST_EFFORT.try_with(|_| ()).is_ok() {
            if let Some(thinking) = req.input.thinking.as_mut() {
                thinking.effort = None;
            }
        }
        req.set_tool_choice(tool_choice);
        req.input.stop_sequences = stop_sequences;
        req.execution.capture_retry_count = true;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.execution.query_source = query_source.map(str::to_string);
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        if query_source == Some("agent_classifier") {
            // Native yF only sends an explicitly supplied temperature and
            // preserves thinking:false as a disabled protocol block.
            req.input.temperature = temperature;
            if req.execution.side_thinking_disabled {
                req.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
                    mode: Some(lingxi_llm_client::protocol::ThinkingMode::Disabled),
                    ..Default::default()
                });
            }
        }
        req.set_effort(effort)?;
        crate::anthropic_extra_body::capture(&mut req)?;
        Ok(req)
    }

    /// Build the canonical strict JSON-schema request used by both estimation
    /// and the live Session dispatch path.
    #[allow(clippy::too_many_arguments)]
    pub fn build_json_schema_request_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        system_input: Option<&crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<LlmRequest, LlmError> {
        let system = side_query_system_prompt(system, system_input)?;
        let mut req = self.build_json_schema_request(
            model,
            profile,
            system.as_ref(),
            messages,
            schema,
            max_tokens,
            query_source,
        )?;
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        req.set_effort(effort)?;
        if query_source.is_some() {
            req.execution.query_source = query_source.map(str::to_string);
        }
        crate::anthropic_extra_body::capture(&mut req)?;
        Ok(req)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_json_schema_request(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        query_source: Option<&str>,
    ) -> Result<LlmRequest, LlmError> {
        let streaming = self
            .resolve_media_route(model, profile)?
            .main
            .capabilities
            .streaming;
        let mut req = self.build_request(
            model,
            profile,
            system,
            messages,
            Vec::new(),
            streaming,
            max_tokens,
            false,
            prompt_cache_query_source(query_source),
        )?;
        req.input.tool_choice = lingxi_llm_client::protocol::ToolChoice::Auto;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonSchema {
            name: "response".into(),
            schema: lingxi_llm_client::protocol::JsonPayload::from_generated_value(&schema)
                .map_err(|error| LlmError::InvalidRequest {
                    message: format!("JSON schema could not be encoded as source: {error}"),
                })?,
            strict: true,
        };
        Ok(req)
    }

    /// Return one completed auxiliary response using the transport selected by
    /// the canonical request builder. Partial structured output is unusable.
    pub async fn complete_side_query_request(
        &self,
        request: LlmRequest,
    ) -> Result<HistoryResponse, LlmError> {
        if request.stream {
            let stream = self.stream_request(request).await?;
            crate::stream_accumulator::accumulate_stream_salvaging(stream)
                .await
                .map_err(|(_, error)| error)
        } else {
            self.execute_side_query_request(request).await
        }
    }

    /// Complete a structured-output query with the session's inherited thinking policy.
    pub async fn complete_json_schema(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = self.build_json_schema_request(
                model,
                profile,
                system.as_ref(),
                messages,
                schema,
                max_tokens,
                None,
            )?;
            req.set_effort(effort)?;
            self.complete_side_query_request(req).await
        })
        .await
    }

    /// Run a session-bound side query through the same request builder,
    /// provider routing, credentials, cache layout, headers, and retry driver
    /// as the parent conversation.
    ///
    /// This is the compaction/recap path. It deliberately replaces the main
    /// request's forced tool choice (for example `--json-schema`) with the side
    /// query's own choice: Claude Code's compaction call exposes no tools and
    /// must not inherit a main-turn `StructuredOutput` requirement. All other
    /// session-scoped wire behavior, including thinking configuration and
    /// request metadata, remains shared.
    ///
    /// `max_tokens` is `None` for the model-aware ordinary request budget — the
    /// same signal the main turn passes. The selected route's output ceiling
    /// is bounded by the available context before dispatch. It was
    /// previously a bare `u32`, which forced every caller to invent a ceiling;
    /// on a reasoning model that invented number silently capped the THINKING
    /// pass as well as the answer. Pass `Some(n)` only where a caller has a real
    /// reason to request a different budget.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<HistoryResponse, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = crate::thinking_scope::isolated(|| {
                self.build_request(
                    model,
                    profile,
                    system.as_ref(),
                    messages,
                    tools,
                    false,
                    max_tokens,
                    false,
                    prompt_cache_query_source(query_source),
                )
            })?;

            // `build_request` applies main-turn-only overrides. A forked summary
            // owns these fields independently, so restore its explicit values.
            req.set_tool_choice(tool_choice);
            req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
            req.input.stop_sequences = stop_sequences;
            req.input.temperature = temperature;
            req.execution.query_source = query_source.map(str::to_string);

            let ctl = resolve_retry_control_with_settings(
                model,
                None,
                self.effective_subscriber().is_subscriber,
                &ResolveRetryEnv::from_process_env(),
                self.settings_max_retries,
            );
            self.drive_non_stream(req, ctl, DispatchHeaderState::AUXILIARY)
                .await
        })
        .await
    }

    /// Non-streaming side query with explicit thinking semantics.
    ///
    /// `thinking = None` means emit no reasoning field at all; `Some(cfg)`
    /// resolves through the same model-specific thinking policy as the main
    /// request path.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        system_input: Option<&crate::SystemPromptInput>,
        skip_global_cache_for_system_prompt: bool,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        effort: Option<serde_json::Value>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<HistoryResponse, LlmError> {
        self.with_account_call_snapshot(async {
            let req = self.build_side_query_request_with_thinking(
                model,
                profile,
                system,
                system_input,
                skip_global_cache_for_system_prompt,
                messages,
                tools,
                max_tokens,
                tool_choice,
                stop_sequences,
                thinking,
                effort,
                temperature,
                query_source,
                std::collections::HashSet::new(),
                std::collections::HashMap::new(),
            )?;

            let ctl = resolve_retry_control_with_settings(
                model,
                None,
                self.effective_subscriber().is_subscriber,
                &ResolveRetryEnv::from_process_env(),
                self.settings_max_retries,
            );
            self.drive_non_stream(req, ctl, DispatchHeaderState::AUXILIARY)
                .await
        })
        .await
    }

    /// Open a session-bound streaming side query for bounded embedded clients.
    /// This mirrors [`Self::messages_create_side_query`] while preserving the
    /// caller's output and temperature limits on the streaming request.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query_stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = crate::thinking_scope::isolated(|| {
                self.build_request(
                    model,
                    profile,
                    system.as_ref(),
                    messages,
                    tools,
                    true,
                    max_tokens,
                    false,
                    prompt_cache_query_source(query_source),
                )
            })?;
            req.set_tool_choice(tool_choice);
            req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
            req.input.stop_sequences = stop_sequences;
            req.input.temperature = temperature;
            req.execution.query_source = query_source.map(str::to_string);
            self.drive_stream(req).await
        })
        .await
    }

    /// Streaming side query with explicit thinking semantics.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query_stream_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = crate::thinking_scope::isolated(|| {
                self.build_request(
                    model,
                    profile,
                    system.as_ref(),
                    messages,
                    tools,
                    true,
                    max_tokens,
                    false,
                    prompt_cache_query_source(query_source),
                )
            })?;
            req.set_tool_choice(tool_choice);
            req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
            req.input.stop_sequences = stop_sequences;
            req.execution.query_source = query_source.map(str::to_string);
            self.apply_side_query_thinking(&mut req, model, thinking, temperature);
            self.drive_stream(req).await
        })
        .await
    }

    /// Structured-output streaming call constrained by a JSON SCHEMA rather
    /// than by a forced tool.
    ///
    /// This is claude-code's `sideQuery` shape: the model is told to emit a
    /// document matching `schema`, which on Anthropic rides the
    /// `structured-outputs` beta (`output_config`) and on OpenAI the stable
    /// `response_format` key. Both encodings live in their provider codecs.
    ///
    /// # Errors
    /// Propagates request-building and transport errors, including a provider
    /// whose codec has no encoding for structured output.
    pub async fn stream_json_schema(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = self.build_json_schema_request(
                model,
                profile,
                system.as_ref(),
                messages,
                schema,
                max_tokens,
                None,
            )?;
            req.set_effort(effort)?;
            self.stream_request(req).await
        })
        .await
    }

    /// [`Self::stream_json_schema`] with explicit thinking and temperature
    /// semantics (F003 round 2), mirroring
    /// [`Self::messages_create_side_query_stream_with_thinking`].
    ///
    /// Plain `stream_json_schema` never touches `req.reasoning` or
    /// `req.input.temperature` at all — it just inherits whatever `build_request`
    /// already derives from the LIVE session thinking config. That is
    /// correct for a caller (like the auto-mode propose query) that wants to
    /// inherit the session's thinking decision untouched. It is wrong for a
    /// caller that wants a specific temperature contract (e.g. the fusion
    /// analyst's `temperature: 0.0`): `build_request` defaults session
    /// thinking to `Adaptive` (ON), so setting a bare temperature override on
    /// top of that inherited config could emit BOTH a `thinking` block and a
    /// non-1.0 `temperature` — a pairing every provider that supports
    /// extended thinking rejects.
    ///
    /// Use this method whenever the caller has an opinion about `thinking`
    /// (including "none at all" — pass `None`, which clears any
    /// session-derived reasoning, exactly like the `sidequery` crate's
    /// `SideQueryRequest.thinking: None` convention) and/or a specific
    /// `temperature` contract, rather than raw `stream_json_schema`.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_json_schema_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        system_input: Option<&crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let req = self.build_json_schema_request_with_thinking(
                model,
                profile,
                system,
                system_input,
                messages,
                schema,
                max_tokens,
                effort,
                thinking,
                temperature,
                query_source,
            )?;
            self.stream_request(req).await
        })
        .await
    }
}
