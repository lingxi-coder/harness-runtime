//! Stream connection, recovery and event delivery.

use crate::dispatch_header::DispatchHeaderState;
use crate::model::retry::{
    resolve_retry_control_with_settings, DriveStep, ResolveRetryEnv, RetryState,
};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::model::telemetry;
use crate::service::anthropic::thinking::NativeThinkingResumptionRefusalLatch;
use crate::service::attempt::{boxed_model_future, new_telemetry_id};
use crate::service::conversation::ConversationLease;
use crate::service::request::{
    custom_system_prompt, native_custom_system_prompt_source, query_source_category,
    MessagesCreateRequest, QuerySourceCategory,
};
use crate::service::retry::{
    allows_account_switch, allows_continuation_recovery, allows_route_replay,
    bind_rate_limit_retry_policy, guard_max_tokens_adjustment, reasoning_budget,
};
use crate::service::usage::{frozen_stream_quote, prepared_output_budget_observation};
use crate::service::{ApiService, ACCOUNT_CALL_CLIENT};
use crate::{HistoryEvent, LlmError, LlmRequest, ModelRuntime, PromptCacheQuerySource};
use futures::stream::BoxStream;
use lingxi_core::types::ConversationMessage;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod events;

/// State threaded through the `futures::stream::unfold` loop in `drive_stream`.
struct StreamState {
    allow_replay: bool,
    diagnostics: crate::history::ResponseDiagnostics,
    account_coordinator: Option<crate::accounts::AttemptCoordinator>,
    account_retry_scope: ModelCallRetryScope,
    account_retry_after: Option<Duration>,
    account_authentication: Option<(
        Arc<ModelRuntime>,
        String,
        String,
        Option<u64>,
        Option<crate::Credential>,
    )>,
    account_derived_token: bool,
    per_turn_effort: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    context_usage_prefix: Option<crate::context_usage::ContextUsagePrefix>,
    context_1m_latch: Option<(
        crate::thinking_scope::ThinkingRecoveryScope,
        crate::thinking_scope::Context1mRouteKey,
    )>,
    mark_context_1m_latch_on_message_start: bool,
    safety: crate::safety_observation::SafetyObservation,
    attempt: crate::model_attempt::WireAttempt,
    decoder: crate::history_projection::HistoryProjector,
    frames: Option<lingxi_llm_client::ModelStream>,
    native_stream_capture: Option<crate::native_stream_capture::AttemptCapture>,
    native_thinking_resumption_refused: Arc<NativeThinkingResumptionRefusalLatch>,
    pricing: Option<lingxi_llm_client::FrozenPricing>,
    pricing_model: crate::PricingModelRef,
    server_fallback_lane:
        Option<lingxi_llm_client::providers::anthropic::fallback_request::ServerLane>,
    server_fallback_quote_finalized: bool,
    server_fallback_quote_candidate_seen: bool,
    server_fallback_quote_metadata: Option<lingxi_llm_client::protocol::JsonPayload>,
    server_fallback_quote_estimate: Option<crate::CostEstimate>,
    pending_service_error: Option<LlmError>,
    /// First frame already pulled by the drive loop's dispatch body-phase
    /// lookahead.
    /// Consumed in place of the first `next_frame()` so decoding, watchdog and
    /// error handling stay identical to the un-seeded path. `None` on every
    /// stream that did not carry `anthropic-dispatch-id`.
    seed: Option<Result<Option<lingxi_llm_client::StreamBatch>, LlmError>>,
    queue: VecDeque<HistoryEvent>,
    finished: bool,
    /// Guard against double-emit: once we have fired succeed/fail we never fire again.
    done: bool,
    /// Optional analytics bus for stream telemetry twins (`emit_succeeded` / `emit_failed`).
    analytics: Option<Arc<::telemetry::AnalyticsBus>>,
    /// Request model string for telemetry labels.
    model: String,
    /// Client-side request id for telemetry correlation.
    request_id: String,
    /// Wall-clock start of the stream for `duration_ms`.
    started: Instant,
    /// Streaming idle watchdog (cc 2.1.196 default-on). `Some(timeout)` when
    /// the watchdog is enabled: each blocking frame read is bounded by this
    /// duration and, on elapse, the stream yields a watchdog
    /// [`LlmError::StreamInterrupted`] (detectable via
    /// [`crate::model::stream_watchdog::is_stream_idle_timeout`]). `None`
    /// disables it (`LINGXI_ENABLE_STREAM_WATCHDOG=0`). The deadline resets on
    /// every received event because a fresh timeout wraps each frame fetch.
    idle_timeout: Option<Duration>,
    /// Absolute classifier deadline shared by headers and every body read.
    classifier_attempt_deadline: Option<tokio::time::Instant>,
    /// Last so the conversation lease returns only after frames release the socket.
    side_ws_lease: Option<ConversationLease>,
}

impl ApiService {
    /// Stream a canonical request through the accounting-aware physical driver.
    pub async fn stream_request(
        &self,
        mut request: LlmRequest,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        request.stream = true;
        self.drive_stream(request).await
    }

    /// Stream a Host subagent request; its COGS label remains separate from
    /// the typed subagent role used by prompt-cache policy.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_with_attempt_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source_label: Option<&str>,
        model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = system.map(native_custom_system_prompt_source).transpose()?;
            let mut request = self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                max_tokens,
                false,
                PromptCacheQuerySource::Subagent,
            )?;
            request.set_effort(effort)?;
            request.execution.query_source = query_source_label.map(str::to_string);
            request.execution.model_attempt = model_attempt;
            // Child tool choice belongs to this request, independently of a
            // shared service's main-loop structured-output configuration.
            request.set_tool_choice(
                forced_tool.map(|name| crate::ToolChoice::Tool { name: name.into() }),
            );
            self.drive_stream_with_wrap_up(request, super::BoundWrapUpHint::current(self))
                .await
        })
        .await
    }

    /// Stream a background memory fork through the existing request builder,
    /// retaining the parent's source vector and identity-keyed prefix metadata.
    /// Ordinary subagents continue to use `stream_with_attempt_opts`.
    pub async fn stream_memory_fork(
        &self,
        mut request: MessagesCreateRequest,
        fallback_system: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        effort: Option<serde_json::Value>,
        forced_tool: Option<String>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            if request.system.is_none() {
                request.system = fallback_system
                    .as_ref()
                    .map(native_custom_system_prompt_source)
                    .transpose()?;
            }
            let (mut request, _) = crate::computer::without_computer_request(|| {
                self.build_main_message_request(request, true)
            })?;
            request.set_effort(effort)?;
            request.set_tool_choice(forced_tool.map(|name| crate::ToolChoice::Tool { name }));
            self.drive_stream_with_wrap_up(request, super::BoundWrapUpHint::current(self))
                .await
        })
        .await
    }

    // ── Stream drive (Step 2) ─────────────────────────────────────────────────

    /// Drive a streaming call; connect-phase failures retry through the driver.
    ///
    /// Uses `ModelRuntime::execute_stream` which already handles the
    /// connect-phase error-drain path internally.  For the retry loop we re-prepare
    /// on each attempt so a fresh `PreparedLlmCall` (with correct auth headers) is
    /// sent even after a previous attempt fails.
    ///
    /// **Streaming rate-limit headers (3c-T1 closed):** `Transport::open_stream`
    /// now returns real `StreamingResponse{status, headers}` via the additive
    /// `stream_sse_with_meta` path added in plan 3c.  The connect-phase ≥400
    /// branch below reads `streaming.headers` and calls `resolve_retry_after`
    /// just as the non-stream path does, so 429+`retry-after` delays are
    /// honoured on the streaming path.
    // The streaming driver owns the same large provider lifecycle as the
    // non-stream driver; bound its callers' futures at the same layer.
    pub(super) fn drive_stream(
        &self,
        req: LlmRequest,
    ) -> crate::BoxFuture<'_, Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>>
    {
        self.drive_stream_with_wrap_up(req, None)
    }

    fn drive_stream_with_wrap_up(
        &self,
        req: LlmRequest,
        wrap_up: Option<super::BoundWrapUpHint>,
    ) -> crate::BoxFuture<'_, Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>>
    {
        Box::pin(async move {
            let mut req = req;
            self.capture_thinking_recovery(&mut req);
            crate::anthropic_extra_body::capture(&mut req)?;
            let scope = if req.execution.side_query_max_retries.is_some() {
                ModelCallRetryScope::default()
            } else {
                ModelCallRetryScope::current_or_new()
            };
            let (client, deadline) = self.account_call_client(&scope, true).await?;
            let owner = Arc::as_ptr(&self.client) as usize;
            ACCOUNT_CALL_CLIENT
                .scope((owner, client), async move {
                    let mut dispatched_route_allows_replay = true;
                    let drive = async {
                        let mut coordinator = None;
                        let result =
                            crate::model::request_trace::stream(boxed_model_future(|| {
                                self.drive_stream_inner(
                                    req,
                                    wrap_up,
                                    &mut dispatched_route_allows_replay,
                                    &mut coordinator,
                                )
                            }))
                            .await;
                        if let Some(coordinator) = coordinator.as_mut() {
                            coordinator.release().await;
                        }
                        result
                    };
                    if let Some(deadline) = deadline {
                        let stream = tokio::time::timeout_at(deadline, scope.run(drive))
                            .await
                            .unwrap_or_else(|_| {
                                Err(LlmError::TransportTimeout {
                                    message: "logical account call deadline exceeded".into(),
                                })
                            })
                            .map_err(|error| {
                                error.with_route_replay(dispatched_route_allows_replay)
                            })?;
                        // The deadline includes response reading. Once an item has
                        // been delivered, this wrapper only ends the stream; it never
                        // re-enters account selection or replays prior output.
                        let bounded: BoxStream<'static, Result<HistoryEvent, LlmError>> =
                            Box::pin(futures::stream::unfold(
                                (Some(stream), scope),
                                move |(stream, retry_scope)| async move {
                                    let mut stream = stream?;
                                    match tokio::time::timeout_at(
                                        deadline,
                                        futures::StreamExt::next(&mut stream),
                                    )
                                    .await
                                    {
                                        Ok(Some(item)) => Some((item, (Some(stream), retry_scope))),
                                        Ok(None) => None,
                                        Err(_) => {
                                            let error = LlmError::TransportTimeout {
                                                message: "logical account call deadline exceeded"
                                                    .into(),
                                            };
                                            let error = error
                                                .with_route_replay(dispatched_route_allows_replay);
                                            retry_scope.record_account_stream_error(&error);
                                            drop(stream);
                                            Some((Err(error), (None, retry_scope)))
                                        }
                                    }
                                },
                            ));
                        Ok(bounded)
                    } else {
                        scope.run(drive).await.map_err(|error| {
                            error.with_route_replay(dispatched_route_allows_replay)
                        })
                    }
                })
                .await
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn drive_stream_inner(
        &self,
        mut req: LlmRequest,
        wrap_up: Option<super::BoundWrapUpHint>,
        dispatched_route_allows_replay: &mut bool,
        account_coordinator: &mut Option<crate::accounts::AttemptCoordinator>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let retain_native_stream_frames = crate::native_stream_capture::requested();
        self.capture_request_session_id(&mut req).await;
        if req.execution.request_credentials.is_none()
            && self.native_api_system_route(&req.input.model, req.profile.as_deref())
        {
            req.execution.request_credentials = Some(crate::RequestCredentials::default());
        }
        let mut safety = crate::safety_observation::SafetyObservation::capture(&mut req);
        if req.execution.input_protocol.is_none() {
            req.execution.input_protocol =
                Some(self.protocol_for_model(&req.input.model, req.profile.as_deref())?);
        }
        let request_id = new_telemetry_id();
        telemetry::emit_started(&self.analytics, &req.input.model, &request_id, true).await;
        if let Some(query_source) = req.execution.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.input.model, query_source).await;
        }

        let sub = self.effective_subscriber();
        let mut state = RetryState::default();
        // Stream path uses settings-based retry control (same precedence as non-stream).
        let mut ctl = resolve_retry_control_with_settings(
            &req.input.model,
            None, // fallback not used on stream connect-phase
            sub.is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        if let Some(max) = req.execution.side_query_max_retries {
            ctl.max_retries = ctl.max_retries.min(max);
        }
        let retry_scope = ModelCallRetryScope::current_or_new();
        *account_coordinator = self.make_account_coordinator(&req, &retry_scope).await?;
        self.configure_api_attempt_budget(&mut req, &retry_scope, account_coordinator.is_some());

        let ctl = retry_scope.configure(&ctl, &mut state);
        let thinking_budget: u32 = reasoning_budget(req.input.thinking.as_ref());
        let context_1m_scope = req.execution.thinking_recovery_scope.clone();
        let mut connections = super::retry::ConnectionRetry::new();
        let logical_reasoning = super::retry::LogicalReasoning::capture(&req);
        let mut max_tokens_adjusted = false;
        let mut any_dispatched = false;
        // Native `oSe` is per query, not per HTTP attempt.
        let mut server_fallback_beta_repair_attempted = false;
        let mut context_1m_ordinary_retry_attempted = false;
        let mut context_1m_repair_key: Option<crate::thinking_scope::Context1mRouteKey> = None;
        let caller_context_1m_suppressed = req.execution.context_1m_beta_suppressed;
        // Reset per-query state and classify the actual source independently
        // of stream/body policy: prompt hooks and scheduled streams are auxiliary.
        let mut dispatch =
            DispatchHeaderState::for_query_source(req.execution.query_source.as_deref());
        dispatch.context_hint_beta = req.execution.context_hint_beta;
        // The conversation's WebSocket belongs to its main turn. Every other
        // stream may run while that turn is still streaming, including a
        // subagent that carries no source label. The orchestrator's streaming
        // turn (`stream_with_system_prompt`) names its source only to the
        // prompt-cache policy, so read it there when the request has no label;
        // a subagent request records none in either place.
        let source = req.execution.query_source.as_deref().or_else(|| {
            req.execution
                .prompt_cache
                .as_ref()
                .and_then(|cache| cache.policy.query_source.as_deref())
        });
        let main_conversation = !req.execution.isolated_query
            && query_source_category(source) == Some(QuerySourceCategory::Main);
        tracing::debug!(target: "llm_runtime::continuation", main_conversation,
            query_source = source, isolated_query = req.execution.isolated_query, "conversation lease");
        let mut conversation_lease = Some(if main_conversation {
            ConversationLease::checkout(&self.responses_ws_session)
        } else {
            ConversationLease::auxiliary(&self.side_sessions)
        });
        let mut continuation_recovered = false;
        let rate_limit_account_epoch = self.rate_limit_account_epoch();

        loop {
            let previous_account = req.execution.selected_account.clone();
            self.select_attempt_account(account_coordinator.as_mut(), &retry_scope, &mut req)
                .await?;
            if previous_account.is_some() && previous_account != req.execution.selected_account {
                logical_reasoning.restore(&mut req);
            }

            self.clamp_scope_output(&mut req, any_dispatched)?;
            self.apply_native_main_thinking_policy(&mut req)?;
            let context_1m_attempt_key = self.context_1m_route_key(&req);
            let mark_context_1m_latch_on_message_start = context_1m_repair_key
                .as_ref()
                .is_some_and(|key| Some(key) == context_1m_attempt_key.as_ref());
            req.execution.context_1m_beta_suppressed = caller_context_1m_suppressed
                || mark_context_1m_latch_on_message_start
                || context_1m_scope
                    .as_ref()
                    .zip(context_1m_attempt_key.as_ref())
                    .is_some_and(|(scope, key)| scope.context_1m_refused_for(key));
            let classifier_timeout = req
                .execution
                .classifier_attempt_timeout_ms
                .map(Duration::from_millis);
            let mut classifier_attempt_deadline =
                classifier_timeout.map(|timeout| tokio::time::Instant::now() + timeout);
            let note = if let Some(hint) = wrap_up.as_ref() {
                match self.client().subagent_wrap_up_note(&mut req, hint).await {
                    Ok(note) => note,
                    Err(error) => {
                        if self
                            .retry_after_preparation_failure(
                                &req,
                                account_coordinator.as_mut(),
                                &error,
                                &retry_scope,
                            )
                            .await?
                        {
                            continue;
                        }
                        return Err(error);
                    }
                }
            } else {
                None
            };
            let (request_client, projected) = crate::execution::classifier_attempt_bound(
                classifier_attempt_deadline,
                self.bind_attempt_route(&mut req, true, any_dispatched, note),
            )
            .await?;
            let mut prepared = match crate::execution::classifier_attempt_bound(
                classifier_attempt_deadline,
                request_client.prepare_on(
                    projected.as_ref().unwrap_or(&req),
                    self.transport.clone(),
                    crate::execution::PreparationDispatchPolicy {
                        quota: self.scope_output_quota.clone(),
                        prior_dispatch: any_dispatched,
                    },
                ),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    if self
                        .retry_after_preparation_failure(
                            &req,
                            account_coordinator.as_mut(),
                            &e,
                            &retry_scope,
                        )
                        .await?
                    {
                        continue;
                    }
                    return Err(e);
                }
            };
            bind_rate_limit_retry_policy(&mut state, sub, &prepared);
            connections.capture(
                &prepared.route.resolved_route,
                account_coordinator.is_none(),
            );
            tracing::debug!(model = %req.input.model, event = "request_prepared");
            let (dispatch_attempt, server_fallback_parameter_added) = crate::execution::classifier_attempt_bound(
                classifier_attempt_deadline,
                self.finish_attempt_preparation(&req, &mut prepared, &retry_scope, dispatch, true),
            )
            .await?;

            let mut allow_replay = allows_route_replay(&req, &prepared);
            let draft = crate::ModelRuntime::shared_stream_draft(&mut prepared)?;
            let first_byte_timeout = if classifier_timeout.is_some() {
                // The absolute classifier deadline below owns both phases.
                None
            } else {
                self.stream_first_byte_timeout_override.or_else(|| {
                    let body_bytes = prepared
                        .provider_request
                        .body_bytes
                        .as_ref()
                        .map_or_else(|| draft.request().body.len(), Vec::len);
                    crate::model::stream_watchdog::resolve_stream_first_byte_timeout(
                        &prepared.route.resolved_route.provider_id,
                        body_bytes,
                    )
                })
            };
            let mut attempt = crate::model_attempt::WireAttempt::new(None);
            let mut admission = super::attempt::AttemptAdmission::new(self, &req);
            let mut rate_limit_headers = None;
            let mut prompt_cache_header_order = None;

            // Open stream through the prepared-call path so injected headers are
            // preserved while OpenAI Responses providers can reuse a WebSocket
            // session and apply previous_response_id deltas. A registered
            // attempt takes the same path: the opener marks dispatch
            // immediately before whichever transport call carries it, so a
            // WebSocket send is metered exactly like an HTTP one.
            let native_stream_capture = (retain_native_stream_frames
                && prepared.route.protocol == crate::ProtocolFamily::AnthropicMessages
                && prepared.route.resolved_route.provider_id
                    == crate::ProviderId::AnthropicFirstParty)
                .then(crate::native_stream_capture::AttemptCapture::default);
            let opened = crate::native_stream_capture::AttemptCapture::scope(
                native_stream_capture.as_ref(),
                async {
                    let responses_ws_session = conversation_lease
                        .as_mut()
                        .expect("conversation lease")
                        .session();
                    let preparing = tokio::time::Instant::now();
                    let prepared = crate::execution::classifier_attempt_bound(
                        classifier_attempt_deadline,
                        crate::execution::first_byte_bound(
                            first_byte_timeout,
                            request_client.prepare_shared_stream(
                                prepared,
                                draft,
                                responses_ws_session,
                            ),
                        ),
                    )
                    .await?;
                    let remaining = first_byte_timeout
                        .map(|timeout| timeout.saturating_sub(preparing.elapsed()));
                    allow_replay = allows_route_replay(&req, &prepared);
                    self.validate_scope_output(&prepared)?;
                    // Budget/queue admission is host work, outside the network
                    // watchdog. Its wait must not masquerade as a provider timeout.
                    let admission_started = tokio::time::Instant::now();
                    attempt = self.begin_model_attempt(&req, &prepared).await?;
                    admission.authorize_quota().await?;
                    if let Some(deadline) = classifier_attempt_deadline.as_mut() {
                        *deadline += admission_started.elapsed();
                    }
                    safety.inherit_if_missing(attempt.model_safety_observer());
                    let observations = super::attempt::DispatchObservations::capture(&prepared);
                    crate::execution::classifier_attempt_bound(
                        classifier_attempt_deadline,
                        crate::execution::first_byte_bound(remaining, async {
                            admission.check_computer_submission()?;
                            request_client
                                .open_shared_stream(
                                    prepared,
                                    responses_ws_session,
                                    &mut |prepared| {
                                        admission.mark_dispatch(
                                            &mut attempt,
                                            &observations,
                                            &mut any_dispatched,
                                        )?;
                                        if !allow_replay {
                                            retry_scope.forbid_route_replay();
                                        }
                                        *dispatched_route_allows_replay = allow_replay;
                                        Ok(self.admit_rate_limit_route(
                                            rate_limit_account_epoch,
                                            prepared,
                                        ))
                                    },
                                    &mut |binding, response| {
                                        prompt_cache_header_order =
                                            Some(self.anthropic.prompt_cache_overage.observe_headers());
                                        rate_limit_headers = self
                                            .observe_rate_limit_headers(binding, response.status());
                                    },
                                )
                                .await
                        }),
                    )
                    .await
                },
            )
            .await;
            match opened {
                Err(mut transport_err) => {
                    Self::classify_native_invalid_request_413(&mut transport_err);
                    self.observe_native_thinking_resumption_refusal(&transport_err);
                    safety.error(&transport_err);
                    if admission.rejected {
                        attempt.finish().await?;
                        return Err(LlmError::RequestDispatchRejected {
                            prior_dispatch: any_dispatched,
                        });
                    }
                    attempt.finish().await?;
                    if let Some(retried) = self
                        .retry_proxy_auth_helper(
                            &transport_err,
                            &retry_scope,
                            &mut state,
                            &ctl,
                            thinking_budget,
                        )
                        .await
                    {
                        if retried {
                            continue;
                        }
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            Self::status_of(&transport_err),
                        )
                        .await;
                        return Err(transport_err);
                    }
                    if transport_err.is_proxy_tunnel_auth_rejection() {
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            Self::status_of(&transport_err),
                        )
                        .await;
                        return Err(transport_err);
                    }
                    if !allow_replay {
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            Self::status_of(&transport_err),
                        )
                        .await;
                        return Err(transport_err);
                    }
                    // The oracle permits one `StreamNoResponse` retry across the
                    // whole request, then terminates before generic retry logic.
                    // On the first occurrence it still flows through dispatch
                    // degradation and the normal retry/backoff classifier.
                    if crate::model::stream_watchdog::is_stream_no_response(&transport_err)
                        && !retry_scope.take_no_response_retry()
                    {
                        return Err(transport_err);
                    }
                    if account_coordinator.is_some()
                        && matches!(
                            transport_err.classification_source(),
                            LlmError::Transport { .. }
                                | LlmError::TransportTimeout { .. }
                                | LlmError::StreamInterrupted { .. }
                        )
                    {
                        return Err(transport_err);
                    }
                    if let Some(fallback) = Self::note_dispatch_header_failure(
                        &mut dispatch,
                        &dispatch_attempt,
                        &transport_err,
                        None,
                    ) {
                        self.report_dispatch_fallback(&req, fallback, None).await;
                        continue;
                    }
                    if let Some(coordinator) = account_coordinator.as_mut() {
                        match self
                            .refresh_pool_account(&mut req, &retry_scope, &transport_err)
                            .await
                        {
                            Ok(true) => continue,
                            Err(refresh_error) => {
                                self.record_pool_refresh_failure(&req, coordinator, &refresh_error)
                                    .await?;
                                retry_scope
                                    .mark_account_failed(coordinator.last_failed_account_key());
                                if allows_account_switch(&req) {
                                    continue;
                                }
                                return Err(refresh_error.error);
                            }
                            Ok(false) => {}
                        }
                        if self
                            .record_pool_failure(&req, coordinator, &transport_err, None)
                            .await?
                        {
                            retry_scope.mark_account_failed(coordinator.last_failed_account_key());
                            if allows_account_switch(&req) {
                                continue;
                            }
                            return Err(transport_err);
                        }
                        // Unknown transport settlement cannot be replayed under a
                        // different account or by the standalone retry driver.
                        if matches!(
                            transport_err.classification_source(),
                            LlmError::Transport { .. } | LlmError::TransportTimeout { .. }
                        ) {
                            return Err(transport_err);
                        }
                    }
                    if let Some(next) =
                        connections.advance(&mut req, &mut state, &retry_scope, &transport_err)
                    {
                        logical_reasoning.restore(&mut req);
                        tracing::info!(
                            event = "connection_failover",
                            next_connection = %next,
                            "endpoint failed; retrying the same model on the next connection"
                        );
                        continue;
                    }
                    let step = retry_scope.next_step(
                        &mut state,
                        &ctl,
                        &transport_err,
                        thinking_budget,
                        self.settings_backoff_ms,
                    );
                    match step {
                        DriveStep::RetryAfter(delay) => {
                            self.report_and_sleep_retry(&transport_err, delay, &state, &ctl)
                                .await;
                            continue;
                        }
                        _ => return Err(transport_err),
                    }
                }
                Ok((mut prepared, streaming, continuation_rejection)) => {
                    let outgoing_client_request_id =
                        streaming.client_request_id().map(str::to_owned);
                    let response_headers: std::collections::BTreeMap<String, String> = streaming
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                        .collect();
                    tracing::debug!(
                        model = %req.input.model,
                        event = "stream_opened",
                        status = streaming.status()
                    );
                    // Connect-phase status ≥ 400: drain and decode as error.
                    if streaming.status() >= 400 || continuation_rejection.is_some() {
                        let response_status = streaming.status();
                        // Error responses do not enter the event watchdog below.
                        // Bound their body collection too, respecting the host's
                        // explicit watchdog-disable setting.
                        let timeout = self
                            .stream_idle_timeout_override
                            .or_else(crate::model::stream_watchdog::resolve_stream_idle_timeout);
                        let collect =
                            async { streaming.collect().await.map_err(crate::upstream::error) };
                        let result = crate::execution::classifier_attempt_bound(
                            classifier_attempt_deadline,
                            async {
                                match timeout {
                                    Some(timeout) => tokio::time::timeout(timeout, collect)
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err(LlmError::TransportTimeout {
                                                message:
                                                    "Timed out reading provider error response"
                                                        .into(),
                                            })
                                        }),
                                    None => collect.await,
                                }
                            },
                        )
                        .await;
                        let collected = match result {
                            Ok(collected) => collected,
                            Err(error) => {
                                self.observe_native_thinking_resumption_refusal(&error);
                                safety.error(&error);
                                attempt.finish().await?;
                                // A classifier has not received a stream yet;
                                // its outer accumulator cannot retry this body
                                // failure. Spend the same ledger here while
                                // preserving the server's retry-decline hint.
                                if classifier_timeout.is_some()
                                    && allow_replay
                                    && !crate::model::retry_scope::http_retry_decline_is_terminal(
                                        response_status,
                                        response_headers
                                            .get("x-should-retry")
                                            .is_some_and(|value| value == "false"),
                                        false,
                                        crate::model::retry::retry_watchdog_from_env(),
                                    )
                                {
                                    if let DriveStep::RetryAfter(delay) = retry_scope.next_step(
                                        &mut state,
                                        &ctl,
                                        &error,
                                        thinking_budget,
                                        self.settings_backoff_ms,
                                    ) {
                                        self.report_and_sleep_retry(&error, delay, &state, &ctl)
                                            .await;
                                        continue;
                                    }
                                }
                                return Err(error);
                            }
                        };
                        let body_json = crate::execution::response(
                            collected.response(),
                            prepared.route.protocol,
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                        )
                        .body_json;
                        let outgoing_client_request_id =
                            collected.client_request_id().map(str::to_owned);
                        if let Some((usage, completeness)) = crate::upstream::usage(
                            collected.usage_report(),
                            collected.inference_report(),
                        ) {
                            attempt.observe(&usage, completeness);
                        }
                        let mut decode_err = collected
                            .decode()
                            .err()
                            .map(crate::upstream::error)
                            .unwrap_or(LlmError::ProviderInternal);
                        Self::classify_native_invalid_request_413(&mut decode_err);
                        self.observe_native_thinking_resumption_refusal(&decode_err);
                        safety.error(&decode_err);
                        let recover_continuation = !continuation_recovered
                            && allows_continuation_recovery(&req, &prepared)
                            && conversation_lease
                                .as_mut()
                                .expect("conversation lease")
                                .session()
                                .recover(
                                    continuation_rejection
                                        .as_ref()
                                        .unwrap_or(collected.response()),
                                );
                        collected.finish().await;
                        attempt.finish().await?;
                        if recover_continuation {
                            continuation_recovered = true;
                            continue;
                        }
                        // Native streaming Kie handles an AFK-only refusal
                        // before context-1m and TBC recovery. Keep this ahead
                        // of both so an overlapping error removes AFK first.
                        if allow_replay
                            && self.probe_native_stream_afk_error(
                                &prepared,
                                response_status,
                                &body_json,
                            )
                        {
                            continue;
                        }
                        if !context_1m_ordinary_retry_attempted
                            && self.native_context_1m_ordinary_refusal(
                                &prepared,
                                response_status,
                                &body_json,
                            )
                        {
                            context_1m_ordinary_retry_attempted = true;
                            context_1m_repair_key = context_1m_attempt_key.clone();
                            req.execution.context_1m_beta_suppressed = true;
                            continue;
                        }
                        if self.record_native_thinking_type_rejection(
                            &req,
                            &prepared,
                            &decode_err,
                            response_status,
                            &retry_scope,
                        ) {
                            continue;
                        }
                        if Self::native_thinking_binding_controls_qee_refusal(
                            &prepared,
                            &decode_err,
                            response_status,
                        ) {
                            Self::refuse_native_thinking_binding_controls(&mut prepared);
                            continue;
                        }
                        if let Some(retried) = self
                            .retry_proxy_auth_helper(
                                &decode_err,
                                &retry_scope,
                                &mut state,
                                &ctl,
                                thinking_budget,
                            )
                            .await
                        {
                            if retried {
                                continue;
                            }
                            telemetry::emit_failed(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                Self::error_kind(&decode_err),
                                Self::status_of(&decode_err),
                            )
                            .await;
                            return Err(decode_err);
                        }
                        if decode_err.is_proxy_tunnel_auth_rejection() {
                            telemetry::emit_failed(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                Self::error_kind(&decode_err),
                                Self::status_of(&decode_err),
                            )
                            .await;
                            return Err(decode_err);
                        }

                        // Mirror the non-stream path: for 429s, resolve the
                        // actual retry delay from the real response headers
                        // (retry-after / anthropic-ratelimit-*).  Absent hints leave delay selection to the retry policy.
                        let mut effective_err = decode_err.clone();
                        let response_id = crate::execution::extract_response_request_id(
                            prepared.route.protocol,
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                            &response_headers,
                        );
                        let mut rate_limit_message = None;
                        let mut pending_overage = None;
                        let pending_429 = if let LlmError::RateLimited { retry_after, .. } =
                            decode_err.classification_source()
                        {
                            // Task 6 (batch 5): same 429-error-header capture
                            // as the non-stream path (errors.ts:471-516).
                            let observation = self.record_rate_limit_from_429_for_route(
                                &response_headers,
                                Some(&body_json),
                                &prepared.route.resolved_route.request_model,
                                outgoing_client_request_id.as_deref(),
                                prepared.route.protocol,
                                crate::execution::response_provider_id(
                                    &prepared.route.resolved_route.provider_id,
                                ),
                                rate_limit_headers,
                            );
                            rate_limit_message = observation.message;
                            pending_overage = self.stage_prompt_cache_overage_from_429(
                                &response_headers,
                                prepared.prompt_cache.as_ref(),
                                prompt_cache_header_order,
                            );
                            let delay = retry_after.or_else(|| {
                                Self::resolve_retry_after(
                                    &response_headers,
                                    prepared.route.protocol,
                                    crate::execution::response_provider_id(
                                        &prepared.route.resolved_route.provider_id,
                                    ),
                                )
                            });
                            if let LlmError::RateLimited {
                                retry_after: effective_retry_after,
                                ..
                            } = effective_err.classification_source_mut()
                            {
                                *effective_retry_after = delay;
                            }
                            observation.pending
                        } else {
                            None
                        };

                        if Self::repair_server_fallback_beta_rejection(
                            &prepared,
                            response_status,
                            &body_json,
                            server_fallback_parameter_added,
                            server_fallback_beta_repair_attempted,
                        ) {
                            server_fallback_beta_repair_attempted = true;
                            continue;
                        }

                        if allow_replay
                            && self.probe_thinking_display_error(
                                &prepared,
                                &retry_scope,
                                response_status,
                                &body_json,
                            )
                        {
                            continue;
                        }

                        let declined = response_headers
                            .get("x-should-retry")
                            .is_some_and(|value| value == "false");
                        // Native server/overload model gates precede retry hints.
                        // SDK stateful execution still requires an explicit host decision.
                        if allow_replay && account_coordinator.is_none() && retry_scope.request_http_model_fallback(response_status, lingxi_llm_client::providers::anthropic::response_policy::has_overload_payload(&body_json))
                        {
                            return Err(decode_err.with_response_diagnostics(response_id, rate_limit_message));
                        }
                        if allow_replay
                            && self.repair_fast_model_rejection(
                                &mut req,
                                &prepared,
                                response_status,
                                &body_json,
                            )
                        {
                            continue;
                        }
                        if allow_replay {
                            if let Some(fallback) = Self::note_dispatch_header_failure(
                                &mut dispatch,
                                &dispatch_attempt,
                                &decode_err,
                                Some((response_status, declined)),
                            ) {
                                self.report_dispatch_fallback(
                                    &req,
                                    fallback,
                                    crate::execution::extract_response_request_id(
                                        prepared.route.protocol,
                                        crate::execution::response_provider_id(
                                            &prepared.route.resolved_route.provider_id,
                                        ),
                                        &response_headers,
                                    )
                                    .as_deref(),
                                )
                                .await;
                                continue;
                            }
                        }
                        if let Some(retried) = self
                            .recover_provider_rejection(
                                &mut req,
                                &mut prepared,
                                &decode_err,
                                response_status,
                                &body_json,
                                &retry_scope,
                                true,
                            )
                            .await
                        {
                            if retried {
                                continue;
                            }
                            telemetry::emit_failed(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                Self::error_kind(&decode_err),
                                Self::status_of(&decode_err),
                            )
                            .await;
                            return Err(decode_err
                                .with_response_diagnostics(response_id, rate_limit_message));
                        }
                        if !allow_replay
                            || (account_coordinator.is_none() && crate::model::retry_scope::http_retry_decline_is_terminal(
                                response_status,
                                declined,
                                lingxi_llm_client::providers::anthropic::response_policy::has_overload_payload(&body_json),
                                crate::model::retry::retry_watchdog_from_env(),
                            ))
                        {
                            if matches!(
                                decode_err.classification_source(),
                                LlmError::RateLimited { .. }
                            ) {
                                self.promote_pending_429(pending_429, pending_overage);
                            }
                            telemetry::emit_failed(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                Self::error_kind(&decode_err),
                                Self::status_of(&decode_err),
                            )
                            .await;
                            return Err(decode_err.with_response_diagnostics(response_id, rate_limit_message));
                        }

                        // 2.1.198 `V_c`/`G_c`/`s_f` + `Ygf` — stream connect
                        // twin of the non-stream AWS auth-refresh hook (see
                        // `drive_non_stream_seeded_with_chain`).
                        if let Some(aws) = &self.anthropic.aws_auth {
                            if crate::auth::external_aws::is_aws_auth_error(
                                &decode_err,
                                &prepared.route.resolved_route.provider_id,
                            ) && retry_scope.take_credential_renewal()
                            {
                                aws.refresh().await;
                                continue;
                            }
                        }

                        if let Some(coordinator) = account_coordinator.as_mut() {
                            match self
                                .refresh_pool_account(&mut req, &retry_scope, &effective_err)
                                .await
                            {
                                Ok(true) => continue,
                                Err(refresh_error) => {
                                    self.record_pool_refresh_failure(
                                        &req,
                                        coordinator,
                                        &refresh_error,
                                    )
                                    .await?;
                                    retry_scope
                                        .mark_account_failed(coordinator.last_failed_account_key());
                                    if allows_account_switch(&req) {
                                        continue;
                                    }
                                    return Err(refresh_error.error);
                                }
                                Ok(false) => {}
                            }
                            if self
                                .record_pool_failure(
                                    &req,
                                    coordinator,
                                    &effective_err,
                                    Self::resolve_retry_after(
                                        &response_headers,
                                        prepared.route.protocol,
                                        crate::execution::response_provider_id(
                                            &prepared.route.resolved_route.provider_id,
                                        ),
                                    ),
                                )
                                .await?
                            {
                                retry_scope
                                    .mark_account_failed(coordinator.last_failed_account_key());
                                if allows_account_switch(&req) {
                                    continue;
                                }
                                return Err(effective_err
                                    .with_response_diagnostics(response_id, rate_limit_message));
                            }
                            // Unknown transport settlement cannot be replayed under a
                            // different account or by the standalone retry driver.
                            if matches!(
                                effective_err.classification_source(),
                                LlmError::Transport { .. } | LlmError::TransportTimeout { .. }
                            ) {
                                return Err(effective_err
                                    .with_response_diagnostics(response_id, rate_limit_message));
                            }
                        }
                        if let Some(next) =
                            connections.advance(&mut req, &mut state, &retry_scope, &effective_err)
                        {
                            logical_reasoning.restore(&mut req);
                            tracing::info!(
                                event = "connection_failover",
                                next_connection = %next,
                                "endpoint failed; retrying the same model on the next connection"
                            );
                            continue;
                        }
                        let step = guard_max_tokens_adjustment(
                            retry_scope.next_step(
                                &mut state,
                                &ctl,
                                &effective_err,
                                thinking_budget,
                                self.settings_backoff_ms,
                            ),
                            req.input.max_tokens,
                            max_tokens_adjusted,
                        );
                        match step {
                            DriveStep::RetryAfter(delay) => {
                                self.record_prompt_cache_overage_from_quota_wait(
                                    response_status,
                                    &response_headers,
                                    ctl.watchdog,
                                    prepared.prompt_cache.as_ref(),
                                    prompt_cache_header_order,
                                );
                                self.report_and_sleep_retry(&effective_err, delay, &state, &ctl)
                                    .await;
                                // Re-prepare on next iteration so headers stay fresh.
                                continue;
                            }
                            DriveStep::AdjustMaxTokens(new_max) => {
                                if let LlmError::InvalidRequest { message } =
                                    decode_err.classification_source()
                                {
                                    if let Some(overflow) =
                                        crate::model::overflow::parse_overflow_message(message)
                                    {
                                        telemetry::emit_max_tokens_overflow_adjustment(
                                            &self.analytics,
                                            &req.input.model,
                                            overflow.input_tokens,
                                            overflow.context_limit,
                                            new_max,
                                            state.attempt,
                                        )
                                        .await;
                                    }
                                }
                                max_tokens_adjusted = true;
                                req.input.max_tokens = Some(new_max);
                                continue;
                            }
                            DriveStep::StripThinkingSignature => {
                                if !req.execution.resume_incomplete_thinking()
                                    && self.handle_thinking_signature_strip(&mut req).await
                                {
                                    continue;
                                }
                            }
                            _ => {}
                        }
                        // B6-T1: the stream connect DIES here — promote the
                        // 429 snapshot staged this attempt (TS terminal catch
                        // handler, claudeAiLimits.ts:487). Gated on the
                        // RateLimited discriminant so a non-429 terminal never
                        // promotes a stale slot.
                        if matches!(
                            decode_err.classification_source(),
                            LlmError::RateLimited { .. }
                        ) {
                            self.promote_pending_429(pending_429, pending_overage);
                        }
                        // Terminal twin for the connect-phase emit_started
                        // (mirrors the non-stream terminal arms).
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&decode_err),
                            Self::status_of(&decode_err),
                        )
                        .await;
                        return Err(
                            decode_err.with_response_diagnostics(response_id, rate_limit_message)
                        );
                    }

                    let provider_request_id = self.record_rate_limit_from_headers_for_route(
                        &response_headers,
                        outgoing_client_request_id.as_deref(),
                        prepared.route.protocol,
                        crate::execution::response_provider_id(
                            &prepared.route.resolved_route.provider_id,
                        ),
                        rate_limit_headers,
                    );
                    self.record_prompt_cache_overage_from_headers(
                        &response_headers,
                        prepared.prompt_cache.as_ref(),
                        prompt_cache_header_order,
                    );
                    let diagnostics = crate::history::ResponseDiagnostics::new(
                        provider_request_id,
                        retry_scope.retry_count(),
                    );
                    *self.last_retry_count.lock().unwrap() = diagnostics.retries;
                    let response_metadata =
                        lingxi_llm_client::providers::response_headers::stream_metadata_for_route(
                            crate::upstream::family(&prepared.route.protocol),
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                            &response_headers
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect::<Vec<_>>(),
                        );
                    let response_metadata = lingxi_llm_client::protocol::JsonPayload::parse(
                        &response_metadata.to_string(),
                    )
                    .map_err(|error| LlmError::InvalidRequest {
                        message: error.to_string(),
                    })?;
                    let mut decoder = crate::history_projection::HistoryProjector::projection(
                        crate::upstream::family(&prepared.route.protocol),
                        response_metadata,
                    )?;
                    decoder.local_tool_search_admitted = prepared.local_tool_search_admitted;
                    crate::history_projection::attach_output_budget(
                        &mut decoder.metadata,
                        prepared_output_budget_observation(&prepared, &req.input, allow_replay),
                    )?;
                    if let Some(binding) = prepared.computer_binding.as_ref() {
                        crate::history_projection::attach_computer_binding(
                            &mut decoder.metadata,
                            binding,
                        )?;
                    }
                    decoder.admit_server_fallback(
                        prepared.server_fallback_lane.clone(),
                        prepared.route.resolved_route.profile_name.clone(),
                        prepared.route.resolved_route.request_model.clone(),
                        crate::execution::extract_response_request_id(
                            prepared.route.protocol,
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                            &response_headers,
                        ),
                    )?;
                    if native_stream_capture.is_some() {
                        crate::history_projection::attach_prepared_thinking_display(
                            &mut decoder.metadata,
                            &prepared.provider_request,
                        )?;
                    }
                    let mut frames = streaming
                        .into_stream()
                        .map_err(|_| LlmError::ProviderInternal)?;
                    let pricing_model = prepared.route.resolved_route.pricing_model.clone();
                    let pricing = frames
                        .pricing_snapshot()
                        .filter(|_| {
                            self.estimator.is_some() || req.execution.model_attempt.is_some()
                        })
                        .map(|snapshot| {
                            self.estimator.as_ref().map_or_else(
                                || snapshot.clone(),
                                |estimator| estimator.capture(snapshot.clone(), &pricing_model),
                            )
                        });

                    Self::settle_thinking_display_probe(&prepared, &retry_scope);

                    // Clone analytics + metadata into the unfold state so
                    // emit_succeeded / emit_failed can fire from inside the async closure.
                    let stream_started = Instant::now();
                    let stream_analytics = self.analytics.clone();
                    let stream_model = req.input.model.clone();
                    let stream_request_id = request_id.clone();
                    // Streaming idle watchdog (cc 2.1.196 default-on): resolve
                    // the per-event idle timeout from the env once at
                    // stream-open. `None` when disabled.
                    let stream_idle_timeout = self
                        .stream_idle_timeout_override
                        .or_else(crate::model::stream_watchdog::resolve_stream_idle_timeout);

                    // Native body-phase recovery requires a carried dispatch header
                    // and a connection failure before the first forwarded event.
                    let mut seed = None;
                    if (dispatch_attempt.value.is_some() || account_coordinator.is_some())
                        && allow_replay
                    {
                        let first = crate::execution::classifier_attempt_bound(
                            classifier_attempt_deadline,
                            async {
                                match stream_idle_timeout {
                                    Some(t) => {
                                        // Wall clock across the same wait the monotonic
                                        // timeout bounds: monotonic time stops while the
                                        // machine is suspended, so the excess IS the sleep.
                                        let wall = std::time::SystemTime::now();
                                        match tokio::time::timeout(
                                            t,
                                            crate::execution::next_batch_with_capture(
                                                &mut frames,
                                                native_stream_capture.as_ref(),
                                            ),
                                        )
                                        .await
                                        {
                                            Ok(r) => r,
                                            Err(_elapsed) => Err(
                                                crate::model::stream_watchdog::watchdog_abort_error(
                                                    t,
                                                    wall.elapsed().unwrap_or(t),
                                                ),
                                            ),
                                        }
                                    }
                                    None => {
                                        crate::execution::next_batch_with_capture(
                                            &mut frames,
                                            native_stream_capture.as_ref(),
                                        )
                                        .await
                                    }
                                }
                            },
                        )
                        .await;
                        if let Ok(Some(batch)) = &first {
                            if let Some((mut usage, completeness)) =
                                crate::upstream::usage(&batch.usage, &batch.inference)
                            {
                                // An admitted server lane can still resolve to
                                // an ordinary response. Defer its quote until
                                // typed fallback iterations arrive or the
                                // terminal boundary proves there is no cNe
                                // branch; otherwise a later native quote would
                                // be preceded by a false request-model total.
                                usage.cost_estimate = if prepared.server_fallback_lane.is_none() {
                                    frozen_stream_quote(
                                        pricing.as_ref(),
                                        &pricing_model,
                                        &batch.usage,
                                        &batch.inference,
                                    )
                                } else {
                                    None
                                };
                                attempt.observe(&usage, completeness);
                            }
                        }
                        let first = match first {
                            Ok(Some(batch))
                                if batch.usage.usage.is_none()
                                    && batch.events.len() == 1
                                    && batch.events[0].is_err() =>
                            {
                                Err(crate::upstream::error(
                                    batch.events.into_iter().next().unwrap().unwrap_err(),
                                ))
                            }
                            other => other,
                        };
                        if let Err(first_err) = &first {
                            self.observe_native_thinking_resumption_refusal(first_err);
                            if let Some(coordinator) = account_coordinator.as_mut() {
                                match self
                                    .refresh_pool_account(&mut req, &retry_scope, first_err)
                                    .await
                                {
                                    Ok(true) => {
                                        attempt.finish().await?;
                                        continue;
                                    }
                                    Err(refresh_error) => {
                                        self.record_pool_refresh_failure(
                                            &req,
                                            coordinator,
                                            &refresh_error,
                                        )
                                        .await?;
                                        retry_scope.mark_account_failed(
                                            coordinator.last_failed_account_key(),
                                        );
                                        attempt.finish().await?;
                                        if allows_account_switch(&req) {
                                            continue;
                                        }
                                        return Err(refresh_error.error);
                                    }
                                    Ok(false) => {}
                                }
                                if self
                                    .record_pool_failure(
                                        &req,
                                        coordinator,
                                        first_err,
                                        Self::resolve_retry_after(
                                            &response_headers,
                                            prepared.route.protocol,
                                            crate::execution::response_provider_id(
                                                &prepared.route.resolved_route.provider_id,
                                            ),
                                        ),
                                    )
                                    .await?
                                {
                                    retry_scope
                                        .mark_account_failed(coordinator.last_failed_account_key());
                                    attempt.finish().await?;
                                    if allows_account_switch(&req) {
                                        continue;
                                    }
                                    return Err(first_err.clone().with_response_diagnostics(
                                        diagnostics.provider_request_id(),
                                        None,
                                    ));
                                }
                                // Unknown execution/settlement never authorizes
                                // the dispatch-header repair on pooled calls.
                                attempt.finish().await?;
                                if classifier_timeout.is_some() {
                                    // This private side query has not exposed
                                    // the primed body event to a caller. Its
                                    // retry budget still owns this failure.
                                    if let DriveStep::RetryAfter(delay) = retry_scope.next_step(
                                        &mut state,
                                        &ctl,
                                        first_err,
                                        thinking_budget,
                                        self.settings_backoff_ms,
                                    ) {
                                        self.report_and_sleep_retry(first_err, delay, &state, &ctl)
                                            .await;
                                        continue;
                                    }
                                }
                                return Err(first_err.clone().with_response_diagnostics(
                                    diagnostics.provider_request_id(),
                                    None,
                                ));
                            }
                            // Native x2 body classification requires a connection cause;
                            // SDK timeout/abort errors alone do not supply that cause.
                            if matches!(
                                first_err,
                                LlmError::Transport { .. } | LlmError::TlsCert { .. }
                            ) {
                                if let Some(fallback) = dispatch.on_body_failure(&dispatch_attempt)
                                {
                                    attempt.finish().await?;
                                    self.report_dispatch_fallback(
                                        &req,
                                        fallback,
                                        crate::execution::extract_response_request_id(
                                            prepared.route.protocol,
                                            crate::execution::response_provider_id(
                                                &prepared.route.resolved_route.provider_id,
                                            ),
                                            &response_headers,
                                        )
                                        .as_deref(),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                        }
                        seed = Some(first);
                    }

                    // Assemble events via a manual unfold that drives next_frame + decode.
                    // We keep a queue of pre-decoded events and drain them first.
                    let stream_state = StreamState {
                        allow_replay,
                        diagnostics,
                        account_derived_token: req
                            .profile
                            .as_deref()
                            .is_some_and(|profile| self.client().derives_account_bearer(profile)),
                        account_authentication: req
                            .execution
                            .selected_account
                            .as_ref()
                            .and(req.profile.as_ref())
                            .map(|profile| {
                                (
                                    self.client(),
                                    req.input.model.clone(),
                                    profile.clone(),
                                    Self::captured_request_credential_revision(&req, profile),
                                    Self::captured_request_credential(&req, profile),
                                )
                            }),
                        account_coordinator: account_coordinator.take(),
                        account_retry_scope: retry_scope.clone(),
                        account_retry_after: Self::resolve_retry_after(
                            &response_headers,
                            prepared.route.protocol,
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                        ),
                        per_turn_effort: self.prepared_per_turn_effort(&prepared),
                        context_usage_prefix: req
                            .execution
                            .context_usage_prefix
                            .clone()
                            .filter(|prefix| prefix.matches_prompt(&req)),
                        context_1m_latch: context_1m_scope
                            .clone()
                            .zip(context_1m_attempt_key.clone()),
                        mark_context_1m_latch_on_message_start,
                        safety: safety.clone(),
                        attempt,
                        decoder,
                        frames: Some(frames),
                        native_stream_capture,
                        native_thinking_resumption_refused: self
                            .anthropic
                            .native_thinking_resumption_refused
                            .clone(),
                        pricing,
                        pricing_model,
                        server_fallback_lane: prepared.server_fallback_lane.clone(),
                        server_fallback_quote_finalized: false,
                        server_fallback_quote_candidate_seen: false,
                        server_fallback_quote_metadata: None,
                        server_fallback_quote_estimate: None,
                        pending_service_error: None,
                        seed,
                        queue: VecDeque::new(),
                        finished: false,
                        done: false,
                        analytics: stream_analytics,
                        model: stream_model,
                        request_id: stream_request_id,
                        started: stream_started,
                        idle_timeout: stream_idle_timeout,
                        classifier_attempt_deadline,
                        side_ws_lease: conversation_lease.take(),
                    };

                    let boxed: BoxStream<'static, Result<HistoryEvent, LlmError>> =
                        Box::pin(futures::stream::unfold(stream_state, events::next));
                    return Ok(boxed);
                }
            }
        }
    }

    // ── Inherent streaming entry points ──────────────────────────────────────

    /// Streaming call (provider-neutral). The drive logic of the orchestrator's
    /// `StreamingApiClient::stream` and the subagent's `messages_create_stream`
    /// — `profile` is `None` for the subagent path; `effort` is `None` for the
    /// `StreamingApiClient::stream` path (leaving `req.effort` at its default).
    pub async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        effort: Option<serde_json::Value>,
        speed: Option<String>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = self.build_main_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
                None,
                &[],
                &std::collections::HashSet::new(),
                &std::collections::HashMap::new(),
            )?;
            req.set_effort(effort)?;
            // (fast mode) `Some("fast")` from the main loop lights the fast-mode
            // beta via `beta_context`; `None` keeps the body byte-identical.
            req.set_speed(speed)?;
            self.drive_stream(req).await
        })
        .await
    }

    /// Structured-output streaming call (provider-neutral). The drive logic of
    /// the subagent's `messages_create_stream_forced`: build the request, attach
    /// `effort`, and force `tool_choice` to the named tool so the model must emit
    /// a matching structured call.
    pub async fn stream_forced(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                None,
                false,
                PromptCacheQuerySource::Subagent,
            )?;
            req.set_effort(effort)?;
            if let Some(name) = forced_tool {
                req.set_tool_choice(Some(crate::ToolChoice::Tool {
                    name: name.to_string(),
                }));
            }
            self.drive_stream(req).await
        })
        .await
    }

    /// Like [`Self::stream`], but ALSO threads a per-turn output-token ceiling
    /// and a COGS query-source label onto request execution metadata (the agent
    /// crate's `SubagentApiCallOpts` seam — Fusion panels and other opts-aware
    /// subagent callers). The cache policy receives the typed subagent role,
    /// never the COGS label. `max_tokens: None` and `query_source_label: None` keep the
    /// body byte-identical to [`Self::stream`] (auto-computed ceiling, no
    /// label).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source_label: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                max_tokens,
                false,
                PromptCacheQuerySource::Subagent,
            )?;
            req.set_effort(effort)?;
            req.execution.query_source = query_source_label.map(str::to_string);
            self.drive_stream(req).await
        })
        .await
    }

    /// Like [`Self::stream_forced`], with the same per-turn output-token
    /// ceiling + COGS query-source label as [`Self::stream_with_opts`].
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_forced_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source_label: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let system = custom_system_prompt(system);
            let mut req = self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                max_tokens,
                false,
                PromptCacheQuerySource::Subagent,
            )?;
            req.set_effort(effort)?;
            if let Some(name) = forced_tool {
                req.set_tool_choice(Some(crate::ToolChoice::Tool {
                    name: name.to_string(),
                }));
            }
            req.execution.query_source = query_source_label.map(str::to_string);
            self.drive_stream(req).await
        })
        .await
    }
}
