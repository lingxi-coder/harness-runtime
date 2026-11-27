//! Non-streaming model calls and buffered stream recovery.

use crate::dispatch_header::DispatchHeaderState;
use crate::model::retry::{
    resolve_retry_control_with_settings, DriveStep, ResolveRetryEnv, RetryControl, RetryState,
};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::model::telemetry;
use crate::service::anthropic::recovery::strip_signature_blocks_for_fallback;
use crate::service::attempt::{boxed_model_future, new_telemetry_id};
use crate::service::conversation::ConversationLease;
use crate::service::request::{
    query_source_category, request_query_source_category, ConfiguredFallbackTarget, FallbackPolicy,
    MessagesCreateRequest, NonStreamingRequestClass, NonStreamingRetryOptions, QuerySourceCategory,
};
use crate::service::retry::{
    allows_account_switch, allows_continuation_recovery, allows_route_replay,
    bind_rate_limit_retry_policy, guard_max_tokens_adjustment, reasoning_budget,
};
use crate::service::usage::{
    frozen_server_fallback_quote, frozen_stream_quote, prepared_output_budget_observation,
};
use crate::service::{ApiService, ACCOUNT_CALL_CLIENT};
use crate::{HistoryResponse, LlmError, LlmRequest};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NonStreamDrivePolicy {
    Standard,
    /// Native `/model` probe: no ordinary retry/fallback/effort inheritance or
    /// internal request deadline; retain only its explicit one-shot repairs.
    ModelValidation,
}

impl ApiService {
    // ── Non-stream drive (Step 1 + 1b) ───────────────────────────────────────

    /// Shared non-stream retry driver. Accepts an already-built `LlmRequest` so
    /// purpose-specific canonical request entry points can route here.
    ///
    /// Step 1b: before classifying a retryable 5xx, the driver checks
    /// `x-should-retry: false` — that header makes the response terminal (same
    /// behaviour as api-client `retry.rs:196`).
    ///
    /// `dispatch` classifies the query for the `anthropic-dispatch-id` gate
    /// (`fB(i.querySource)`) and carries this query's `Kt` latch.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn drive_non_stream(
        &self,
        req: LlmRequest,
        retry_control: RetryControl,
        dispatch: DispatchHeaderState,
    ) -> Result<HistoryResponse, LlmError> {
        self.drive_non_stream_seeded_with_chain(
            req,
            retry_control,
            0,
            &[],
            dispatch,
            NonStreamDrivePolicy::Standard,
        )
        .await
    }

    /// Non-stream retry driver with a pre-seeded `consecutive_overloaded` counter
    /// and an optional fallback chain.
    ///
    /// The seed is set to 1 when this call is a non-streaming fallback triggered by
    /// a mid-stream `LlmError::Overloaded` — mirroring TS `claude.ts:2559`
    /// (`initialConsecutive529Errors: is529Error(streamingError) ? 1 : 0`).
    ///
    /// `chain` is the ordered slice of fallback models to walk on consecutive
    /// overload events.  `retry_control` must already carry `chain[0]` as
    /// `fallback_model` (set by [`Self::execute_non_stream_request`]); on
    /// each [`DriveStep::Fallback`] the loop advances `chain_idx` and rebuilds
    /// `retry_control` with `chain[chain_idx]` (or disables fallback when
    /// exhausted).
    // Keep the provider/retry state machine on the heap so every public wrapper
    // does not embed and move its full future on the caller's stack.
    pub(super) fn drive_non_stream_seeded_with_chain<'a>(
        &'a self,
        req: LlmRequest,
        retry_control: RetryControl,
        initial_consecutive_overloaded: u8,
        chain: &'a [ConfiguredFallbackTarget],
        dispatch: DispatchHeaderState,
        policy: NonStreamDrivePolicy,
    ) -> crate::BoxFuture<'a, Result<HistoryResponse, LlmError>> {
        Box::pin(async move {
            let mut req = req;
            if policy == NonStreamDrivePolicy::Standard {
                self.capture_thinking_recovery(&mut req);
            }
            crate::anthropic_extra_body::capture(&mut req)?;
            let scope = if req.execution.side_query_max_retries.is_some() {
                ModelCallRetryScope::default()
            } else {
                ModelCallRetryScope::current_or_new()
            };
            let (client, deadline) = self
                .account_call_client(&scope, policy == NonStreamDrivePolicy::Standard)
                .await?;
            let owner = Arc::as_ptr(&self.client) as usize;
            ACCOUNT_CALL_CLIENT
                .scope((owner, client), async move {
                    let mut dispatched_route_allows_replay = true;
                    let drive = async {
                        let mut coordinator = None;
                        let result =
                            crate::model::request_trace::non_stream(boxed_model_future(|| {
                                self.drive_non_stream_inner(
                                    req,
                                    retry_control,
                                    initial_consecutive_overloaded,
                                    chain,
                                    dispatch,
                                    policy,
                                    &mut coordinator,
                                    &mut dispatched_route_allows_replay,
                                )
                            }))
                            .await;
                        if let Some(coordinator) = coordinator.as_mut() {
                            coordinator.release().await;
                        }
                        result
                    };
                    let result = if let Some(deadline) = deadline {
                        tokio::time::timeout_at(deadline, scope.run(drive))
                            .await
                            .unwrap_or_else(|_| Err(LlmError::TransportTimeout {
                                message: "logical account call deadline exceeded".into(),
                            }))
                    } else {
                        scope.run(drive).await
                    };
                    result.map_err(|error| error.with_route_replay(dispatched_route_allows_replay))
                })
                .await
        })
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn drive_non_stream_inner(
        &self,
        mut req: LlmRequest,
        mut retry_control: RetryControl,
        initial_consecutive_overloaded: u8,
        chain: &[ConfiguredFallbackTarget],
        mut dispatch: DispatchHeaderState,
        policy: NonStreamDrivePolicy,
        account_coordinator: &mut Option<crate::accounts::AttemptCoordinator>,
        dispatched_route_allows_replay: &mut bool,
    ) -> Result<HistoryResponse, LlmError> {
        if policy == NonStreamDrivePolicy::Standard {
            self.capture_request_session_id(&mut req).await;
        }
        if req.execution.request_credentials.is_none()
            && self.native_api_system_route(&req.input.model, req.profile.as_deref())
        {
            req.execution.request_credentials = Some(crate::RequestCredentials::default());
        }
        let mut safety = crate::safety_observation::SafetyObservation::capture(&mut req);
        dispatch.context_hint_beta = req.execution.context_hint_beta;
        if req.execution.query_source.is_some() {
            dispatch.auxiliary = query_source_category(req.execution.query_source.as_deref())
                == Some(QuerySourceCategory::Auxiliary);
        }
        if req.execution.input_protocol.is_none() {
            req.execution.input_protocol =
                Some(self.protocol_for_model(&req.input.model, req.profile.as_deref())?);
        }
        let request_id = new_telemetry_id();
        let started = Instant::now();
        let mut connections = super::retry::ConnectionRetry::new();
        let logical_reasoning = super::retry::LogicalReasoning::capture(&req);
        let caller_profile = req.profile.clone();
        telemetry::emit_started(&self.analytics, &req.input.model, &request_id, false).await;
        if let Some(query_source) = req.execution.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.input.model, query_source).await;
        }

        // The tier belongs to the signed-in Claude.ai credential. Bind it to
        // the selected physical route after each preparation.
        let sub = self.effective_subscriber();
        let mut state = RetryState {
            consecutive_overloaded: initial_consecutive_overloaded,
            ..RetryState::default()
        };
        let retry_scope = match policy {
            NonStreamDrivePolicy::Standard => ModelCallRetryScope::current_or_new(),
            NonStreamDrivePolicy::ModelValidation => ModelCallRetryScope::default(),
        };
        *account_coordinator = self.make_account_coordinator(&req, &retry_scope).await?;
        self.configure_api_attempt_budget(&mut req, &retry_scope, account_coordinator.is_some());
        let context_1m_scope = (policy == NonStreamDrivePolicy::Standard)
            .then(|| req.execution.thinking_recovery_scope.clone())
            .flatten();
        retry_control = retry_scope.configure(&retry_control, &mut state);
        // thinking_budget for telemetry: Adaptive → 0, Enabled{b} → b.
        let thinking_budget: u32 = reasoning_budget(req.input.thinking.as_ref());
        // Index into `chain` for the NEXT fallback entry to use.
        // chain_idx=0 means chain[0] is the current fallback in `retry_control`.
        // After a Fallback step, chain_idx advances to point at the next entry.
        // When chain_idx >= chain.len(), the chain is exhausted.
        let mut chain_idx: usize = 0;
        let mut max_tokens_adjusted = false;
        let mut any_dispatched = false;
        // Native `oSe` is scoped to this logical query and survives every
        // prepare/retry within it; a later query gets a fresh one-shot repair.
        let mut server_fallback_beta_repair_attempted = false;
        let mut model_validation_context_1m_retried = false;
        let mut model_validation_oauth_refreshed = false;
        let mut suppress_context_1m_for_retry = false;
        let mut context_1m_ordinary_retry_attempted = false;
        let mut context_1m_repair_key: Option<crate::thinking_scope::Context1mRouteKey> = None;
        let caller_context_1m_suppressed = req.execution.context_1m_beta_suppressed;
        let timeout = match policy {
            NonStreamDrivePolicy::Standard => {
                Some(match req.execution.classifier_attempt_timeout_ms {
                    Some(milliseconds) => std::time::Duration::from_millis(milliseconds),
                    None => crate::model::request_timeout::non_stream_timeout()?,
                })
            }
            NonStreamDrivePolicy::ModelValidation => None,
        };
        let mut timeout_retries = crate::model::request_timeout::TimeoutRetryCount::default();
        let mut conversation_lease = if policy == NonStreamDrivePolicy::Standard
            && !req.execution.isolated_query
            && request_query_source_category(&req) == Some(QuerySourceCategory::Main)
        {
            ConversationLease::checkout(&self.responses_ws_session)
        } else {
            ConversationLease::auxiliary(&self.side_sessions)
        };
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
            if policy == NonStreamDrivePolicy::Standard {
                self.apply_native_main_thinking_policy(&mut req)?;
            }
            let context_1m_attempt_key = self.context_1m_route_key(&req);
            if policy == NonStreamDrivePolicy::Standard {
                req.execution.context_1m_beta_suppressed = caller_context_1m_suppressed
                    || context_1m_repair_key
                        .as_ref()
                        .is_some_and(|key| Some(key) == context_1m_attempt_key.as_ref())
                    || context_1m_scope
                        .as_ref()
                        .zip(context_1m_attempt_key.as_ref())
                        .is_some_and(|(scope, key)| scope.context_1m_refused_for(key));
            }
            // Strip rejected thinking before encode so Gemini / OpenAI-compat
            // thinking models can prepare. DeepSeek / Kimi skip this.
            // prepare → inject headers → execute.
            let preparing = tokio::time::Instant::now();
            let prepare_future = async {
                let (request_client, projected) = self
                    .bind_attempt_route(
                        &mut req,
                        policy == NonStreamDrivePolicy::Standard,
                        any_dispatched,
                        None,
                    )
                    .await?;
                let mut prepared = request_client
                    .prepare_on(
                        projected.as_ref().unwrap_or(&req),
                        self.transport.clone(),
                        crate::execution::PreparationDispatchPolicy {
                            quota: self.scope_output_quota.clone(),
                            prior_dispatch: any_dispatched,
                        },
                    )
                    .await?;
                if suppress_context_1m_for_retry {
                    Self::remove_context_1m_beta(&mut prepared);
                }
                let (dispatch_attempt, server_fallback_parameter_added) = self
                    .finish_attempt_preparation(&req, &mut prepared, &retry_scope, dispatch, false)
                    .await?;
                request_client
                    .seal_prepared_with_session(&mut prepared, Some(conversation_lease.session()))
                    .await?;
                self.validate_scope_output(&prepared)?;
                Ok((prepared, dispatch_attempt, server_fallback_parameter_added))
            };
            let prepared = match timeout {
                Some(timeout) => crate::execution::non_stream_bound(timeout, prepare_future).await,
                None => prepare_future.await,
            };
            let (mut prepared, dispatch_attempt, server_fallback_parameter_added) = match prepared {
                Ok(prepared) => prepared,
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

                    telemetry::emit_failed(
                        &self.analytics,
                        &req.input.model,
                        &request_id,
                        Self::error_kind(&error),
                        Self::status_of(&error),
                    )
                    .await;
                    return Err(error);
                }
            };
            bind_rate_limit_retry_policy(&mut state, sub, &prepared);
            let allow_replay = allows_route_replay(&req, &prepared);
            connections.capture(
                &prepared.route.resolved_route,
                policy == NonStreamDrivePolicy::Standard && account_coordinator.is_none(),
            );
            // Pause the provider deadline while waiting for host admission.
            let prepare_elapsed = preparing.elapsed();
            let remaining = timeout.map(|timeout| timeout.saturating_sub(prepare_elapsed));
            let mut attempt = self.begin_model_attempt(&req, &prepared).await?;
            safety.inherit_if_missing(attempt.model_safety_observer());
            let mut admission = super::attempt::AttemptAdmission::new(self, &req);
            let quota_authorization = admission.authorize_quota().await;
            let dispatch_started = tokio::time::Instant::now();
            let mut callback_error = None;
            let mut accepted_response = None;
            let mut rate_limit_headers = None;
            let mut prompt_cache_header_order = None;
            let observations = super::attempt::DispatchObservations::capture(&prepared);
            let call = prepared.wire_call.take().expect("sealed call");
            let pricing =
                (self.estimator.is_some() || req.execution.model_attempt.is_some()).then(|| {
                    let snapshot = call.pricing_snapshot();
                    self.estimator.as_ref().map_or_else(
                        || snapshot.clone(),
                        |est| {
                            est.capture(
                                snapshot.clone(),
                                &prepared.route.resolved_route.pricing_model,
                            )
                        },
                    )
                });
            let mut submission = None;
            let dispatch_future = async {
                quota_authorization?;
                admission.check_computer_submission()?;
                submission = prepared.prepare_computer_submission().await?;
                let mut rate_limit_binding = None;
                let received = conversation_lease
                    .session()
                    .dispatch(call, || {
                        admission
                            .mark_dispatch(&mut attempt, &observations, &mut any_dispatched)
                            .map_err(|error| {
                                callback_error = Some(error.clone());
                                crate::execution::wire_error(error)
                            })?;
                        if !allow_replay {
                            retry_scope.forbid_route_replay();
                        }
                        *dispatched_route_allows_replay = allow_replay;
                        rate_limit_binding =
                            self.admit_rate_limit_route(rate_limit_account_epoch, &prepared);
                        if let Some(guard) = submission.as_mut() {
                            guard.dispatched();
                        }
                        Ok(())
                    })
                    .await;
                let received = match received {
                    Ok(received) => received,
                    Err(error) => {
                        return Err(callback_error
                            .clone()
                            .unwrap_or_else(|| crate::upstream::error(error)));
                    }
                };
                prompt_cache_header_order =
                    Some(self.anthropic.prompt_cache_overage.observe_headers());
                rate_limit_headers =
                    self.observe_rate_limit_headers(rate_limit_binding, received.status());
                if (200..300).contains(&received.status()) {
                    let headers = lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                        prepared.route.protocol,
                        crate::execution::response_provider_id(&prepared.route.resolved_route.provider_id),
                        received.headers(),
                        std::time::SystemTime::now(),
                    );
                    accepted_response = Some((received.status(), headers.request_id));
                }
                received.collect().await.map_err(crate::upstream::error)
            };
            let resp_result = match remaining {
                Some(remaining) => {
                    crate::execution::non_stream_bound(remaining, dispatch_future).await
                }
                None => dispatch_future.await,
            };

            if let Some(guard) = submission.as_mut() {
                guard.close().await?;
            }

            match resp_result {
                Err(mut transport_err) => {
                    if let Some((status, request_id)) = accepted_response {
                        if matches!(
                            transport_err.classification_source(),
                            LlmError::Transport { .. }
                                | LlmError::TransportTimeout { .. }
                                | LlmError::StreamInterrupted { .. }
                        ) {
                            transport_err =
                                LlmError::ResponseBodyInterrupted { status, request_id };
                        }
                    }
                    Self::classify_native_invalid_request_413(&mut transport_err);
                    self.observe_native_thinking_resumption_refusal(&transport_err);
                    safety.error(&transport_err);
                    if admission.rejected {
                        attempt.finish().await?;
                        return Err(LlmError::RequestDispatchRejected {
                            prior_dispatch: any_dispatched,
                        });
                    }
                    let timeout_limit_exhausted = timeout.is_some_and(|timeout| {
                        timeout_retries.exhausted_from_environment(
                            req.execution.failed_stream_outlasted_timeout,
                            &transport_err,
                            timeout,
                            prepare_elapsed.saturating_add(dispatch_started.elapsed()),
                        )
                    });
                    attempt.finish().await?;
                    if timeout_limit_exhausted {
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            None,
                        )
                        .await;
                        return Err(transport_err);
                    }
                    if let Some(retried) = self
                        .retry_proxy_auth_helper(
                            &transport_err,
                            &retry_scope,
                            &mut state,
                            &retry_control,
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
                    if transport_err.is_proxy_tunnel_auth_rejection()
                        || matches!(
                            transport_err.classification_source(),
                            LlmError::ResponseBodyLimitExceeded { .. }
                                | LlmError::ResponseBodyInterrupted { .. }
                        )
                    {
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
                    if policy == NonStreamDrivePolicy::ModelValidation {
                        if let Some(fallback) = Self::note_dispatch_header_failure(
                            &mut dispatch,
                            &dispatch_attempt,
                            &transport_err,
                            None,
                        ) {
                            self.report_dispatch_fallback(&req, fallback, None).await;
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
                    // Transport-layer failure; feed into the retry driver.
                    let step = retry_scope.next_step(
                        &mut state,
                        &retry_control,
                        &transport_err,
                        thinking_budget,
                        self.settings_backoff_ms,
                    );
                    if let DriveStep::RetryAfter(delay) = step {
                        self.report_and_sleep_retry(&transport_err, delay, &state, &retry_control)
                            .await;
                        continue;
                    }
                    telemetry::emit_failed(
                        &self.analytics,
                        &req.input.model,
                        &request_id,
                        Self::error_kind(&transport_err),
                        None,
                    )
                    .await;
                    return Err(transport_err);
                }
                Ok(collected) => {
                    let provider_resp = crate::execution::response(
                        collected.response(),
                        prepared.route.protocol,
                        crate::execution::response_provider_id(
                            &prepared.route.resolved_route.provider_id,
                        ),
                    );
                    let outgoing_client_request_id =
                        collected.client_request_id().map(str::to_owned);
                    // Step 1b: x-should-retry: false is terminal for retryable 5xx.
                    let x_should_retry_false = provider_resp
                        .headers
                        .get("x-should-retry")
                        .is_some_and(|v| v.as_str() == "false");

                    let server_fallback_facts = collected.anthropic_fallback();
                    let server_fallback_quote =
                        prepared.server_fallback_lane.as_ref().and_then(|lane| {
                            frozen_server_fallback_quote(
                                pricing.as_ref(),
                                &prepared.route.resolved_route.pricing_model,
                                &lane.model,
                                server_fallback_facts.as_ref(),
                                collected.inference_report(),
                            )
                        });
                    let estimate = match &server_fallback_quote {
                        Some(quote) => quote.estimate.clone(),
                        None => frozen_stream_quote(
                            pricing.as_ref(),
                            &prepared.route.resolved_route.pricing_model,
                            collected.usage_report(),
                            collected.inference_report(),
                        ),
                    };
                    let mut extracted_usage = crate::upstream::usage(
                        collected.usage_report(),
                        collected.inference_report(),
                    );
                    if let Some((usage, completeness)) = &mut extracted_usage {
                        usage.cost_estimate = estimate.clone();
                        if let Some(quote) = &server_fallback_quote {
                            crate::history_projection::attach_server_fallback_cost_quote(
                                &mut usage.provider_metadata,
                                quote.metadata.clone(),
                            )?;
                        }
                        attempt.observe(usage, *completeness);
                    }
                    let decoded = crate::execution::decode(&collected).and_then(|mut decoded| {
                        safety.response(&decoded);
                        let server_event=prepared.server_fallback_lane.as_ref().and_then(|_|lingxi_llm_client::providers::anthropic::fallback_response::project_nonstream(&mut decoded,&prepared.route.resolved_route.request_model,provider_resp.request_id.clone()));
                        let mut response=crate::history_projection::project_response(
                            decoded,
                            provider_resp.clone(),
                            crate::upstream::family(&prepared.route.protocol),
                        )?;
                        if prepared.local_tool_search_admitted {
                            crate::history_projection::admit_local_tool_search(&mut response)?;
                        }
                        if let Some(prefix) = req.execution.context_usage_prefix.as_ref().filter(|prefix| prefix.matches_prompt(&req)) {
                            prefix.attach(&mut response.usage)?;
                        }
                        let budget = prepared_output_budget_observation(&prepared, &req.input, allow_replay);
                        crate::history_projection::attach_output_budget(
                            &mut response.provider_metadata,
                            budget.clone(),
                        )?;
                        crate::history_projection::attach_output_budget(
                            &mut response.usage.provider_metadata,
                            budget,
                        )?;
                        let per_turn_effort = self.prepared_per_turn_effort(&prepared);
                        response.set_per_turn_effort(per_turn_effort.as_ref())?;
                        response.usage.account_usage = account_coordinator
                            .as_ref()
                            .and_then(|coordinator| coordinator.usage_attribution());
                        response.usage.cost_estimate = estimate.clone();
                        if let Some(quote) = &server_fallback_quote {
                            crate::history_projection::attach_server_fallback_cost_quote(
                                &mut response.provider_metadata,
                                quote.metadata.clone(),
                            )?;
                            crate::history_projection::attach_server_fallback_cost_quote(
                                &mut response.usage.provider_metadata,
                                quote.metadata.clone(),
                            )?;
                        }
                        if let (Some(event),Some(lane))=(server_event,prepared.server_fallback_lane.clone()) {
                            let event = serde_json::to_value(crate::history::HistoryServerFallback {
                                event,
                                profile: prepared.route.resolved_route.profile_name.clone(),
                                lane,
                            })
                            .map_err(|error| LlmError::InvalidRequest {
                                message: error.to_string(),
                            })?;
                            let event = lingxi_llm_client::protocol::JsonPayload::from_generated_value(&event)
                                .map_err(|error| LlmError::InvalidRequest {
                                    message: error.to_string(),
                                })?;
                            crate::history_projection::append_observation(
                                &mut response.provider_metadata,
                                "server_fallback_events",
                                event,
                            )?;
                        }
                        Ok(response)
                    });
                    if extracted_usage.is_none() {
                        if let Ok(response) = &decoded {
                            let completeness =
                                if crate::model_attempt::has_usage_report(&response.usage) {
                                    crate::ModelAttemptUsageCompleteness::Complete
                                } else {
                                    crate::ModelAttemptUsageCompleteness::Partial
                                };
                            attempt.observe(&response.usage, completeness);
                        }
                    }
                    let recover_continuation = !continuation_recovered
                        && allows_continuation_recovery(&req, &prepared)
                        && conversation_lease.session().recover(collected.response());
                    attempt.finish().await?;
                    // Settle the attempt before cleanup, but keep the upload
                    // lease until the SDK confirms deletion or times out.
                    collected.finish().await;
                    if recover_continuation {
                        continuation_recovered = true;
                        continue;
                    }
                    match decoded {
                        Ok(mut response) => {
                            if let Some(binding) = prepared.computer_binding.as_ref() {
                                crate::history_projection::attach_computer_binding(
                                    &mut response.provider_metadata,
                                    binding,
                                )?;
                            }
                            Self::settle_thinking_display_probe(&prepared, &retry_scope);
                            // Feed rate-limit headers from every 2xx success response.
                            let provider_request_id = self
                                .record_rate_limit_from_headers_for_route(
                                    &provider_resp.headers,
                                    outgoing_client_request_id.as_deref(),
                                    prepared.route.protocol,
                                    crate::execution::response_provider_id(
                                        &prepared.route.resolved_route.provider_id,
                                    ),
                                    rate_limit_headers,
                                );
                            self.record_prompt_cache_overage_from_headers(
                                &provider_resp.headers,
                                prepared.prompt_cache.as_ref(),
                                prompt_cache_header_order,
                            );
                            // 3c-T3: populate response.cost when an estimator is wired.
                            // Unpriced or unknown models leave response.cost = None — never an error.
                            response.cost = estimate;
                            let elapsed_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            telemetry::emit_succeeded(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                elapsed_ms,
                                provider_resp.status,
                            )
                            .await;
                            let diagnostics = crate::history::ResponseDiagnostics::new(
                                provider_request_id,
                                retry_scope.retry_count(),
                            );
                            response.set_diagnostics(&diagnostics)?;
                            if req.execution.capture_retry_count {
                                if !response.provider_metadata.is_root_object() {
                                    response.provider_metadata =
                                        lingxi_llm_client::protocol::JsonPayload::parse("{}")
                                            .expect("empty metadata object is valid JSON");
                                }
                                let retry_count = serde_json::to_value(retry_scope.retry_count())
                                    .map_err(|error| LlmError::InvalidRequest {
                                    message: error.to_string(),
                                })?;
                                let retry_count =
                                    lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                                        &retry_count,
                                    )
                                    .map_err(|error| {
                                        LlmError::InvalidRequest {
                                            message: error.to_string(),
                                        }
                                    })?;
                                response.provider_metadata = response
                                    .provider_metadata
                                    .with_object_field("_lingxi_retry_count", &retry_count)
                                    .map_err(crate::upstream::error)?;
                            }
                            // #5: surface this drive's retry count to the cost
                            // path via `last_retry_count()`.
                            *self.last_retry_count.lock().unwrap() = retry_scope.retry_count();
                            if context_1m_repair_key
                                .as_ref()
                                .is_some_and(|key| Some(key) == context_1m_attempt_key.as_ref())
                            {
                                if let Some((scope, key)) = context_1m_scope
                                    .as_ref()
                                    .zip(context_1m_attempt_key.clone())
                                {
                                    scope.mark_context_1m_refused_for(key);
                                }
                            }
                            if let Some(coordinator) = account_coordinator.as_mut() {
                                coordinator.succeeded().await;
                            }
                            return Ok(response);
                        }
                        Err(mut decode_err) => {
                            let response_id = provider_resp.request_id.clone();
                            Self::classify_native_invalid_request_413(&mut decode_err);
                            self.observe_native_thinking_resumption_refusal(&decode_err);
                            safety.error(&decode_err);
                            if policy == NonStreamDrivePolicy::Standard
                                && !context_1m_ordinary_retry_attempted
                                && self.native_context_1m_ordinary_refusal(
                                    &prepared,
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                )
                            {
                                context_1m_ordinary_retry_attempted = true;
                                context_1m_repair_key = context_1m_attempt_key.clone();
                                req.execution.context_1m_beta_suppressed = true;
                                continue;
                            }
                            if policy == NonStreamDrivePolicy::Standard
                                && self.record_native_thinking_type_rejection(
                                    &req,
                                    &prepared,
                                    &decode_err,
                                    provider_resp.status,
                                    &retry_scope,
                                )
                            {
                                continue;
                            }
                            if policy == NonStreamDrivePolicy::Standard
                                && Self::native_thinking_binding_controls_qee_refusal(
                                    &prepared,
                                    &decode_err,
                                    provider_resp.status,
                                )
                            {
                                Self::refuse_native_thinking_binding_controls(&mut prepared);
                                continue;
                            }
                            if let Some(retried) = self
                                .retry_proxy_auth_helper(
                                    &decode_err,
                                    &retry_scope,
                                    &mut state,
                                    &retry_control,
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
                                return Err(decode_err.with_response_diagnostics(response_id, None));
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
                                return Err(decode_err.with_response_diagnostics(response_id, None));
                            }
                            if policy == NonStreamDrivePolicy::ModelValidation
                                && !model_validation_context_1m_retried
                                && Self::request_beta_tokens(&prepared)
                                    .iter()
                                    .any(|beta| beta == crate::model::betas::CONTEXT_1M)
                                && self.native_context_1m_resend_allowed(&prepared)
                                && Self::native_context_1m_probe_refusal(
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                )
                            {
                                model_validation_context_1m_retried = true;
                                suppress_context_1m_for_retry = true;
                                req.execution.context_1m_beta_suppressed = true;
                                continue;
                            }
                            if policy == NonStreamDrivePolicy::ModelValidation
                                && !model_validation_oauth_refreshed
                                && (provider_resp.status == 401
                                    || lingxi_llm_client::providers::anthropic::error_recognition::oauth_token_revoked(
                                        provider_resp.status,
                                        decode_err.provider_message(),
                                    ))
                            {
                                let profile = &prepared.route.resolved_route.profile_name;
                                if let Some(rejected) =
                                    Self::captured_request_credential(&req, profile)
                                {
                                    if let Some(refreshed) = self
                                        .client()
                                        .refresh_rejected_anthropic_oauth(
                                            &req.input.model,
                                            Some(profile),
                                            &rejected,
                                        )
                                        .await?
                                    {
                                        model_validation_oauth_refreshed = true;
                                        Self::replace_request_credential(
                                            &mut req,
                                            profile,
                                            crate::auth::provider::VersionedCredential {
                                                credential: refreshed,
                                                credential_revision: None,
                                            },
                                        );
                                        continue;
                                    }
                                }
                            }
                            if policy == NonStreamDrivePolicy::ModelValidation {
                                if self.recover_foundry_tools(
                                    &req,
                                    &prepared,
                                    &decode_err,
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                    &retry_scope,
                                ) {
                                    continue;
                                }
                                if allow_replay {
                                    if let Some(fallback) = Self::note_dispatch_header_failure(
                                        &mut dispatch,
                                        &dispatch_attempt,
                                        &decode_err,
                                        Some((provider_resp.status, x_should_retry_false)),
                                    ) {
                                        self.report_dispatch_fallback(
                                            &req,
                                            fallback,
                                            provider_resp.request_id.as_deref(),
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                telemetry::emit_failed(
                                    &self.analytics,
                                    &req.input.model,
                                    &request_id,
                                    Self::error_kind(&decode_err),
                                    Self::status_of(&decode_err),
                                )
                                .await;
                                return Err(decode_err.with_response_diagnostics(response_id, None));
                            }
                            if Self::repair_server_fallback_beta_rejection(
                                &prepared,
                                provider_resp.status,
                                &provider_resp.body_json,
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
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                )
                            {
                                continue;
                            }
                            if allow_replay
                                && self.repair_fast_model_rejection(
                                    &mut req,
                                    &prepared,
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                )
                            {
                                continue;
                            }
                            if allow_replay {
                                if let Some(fallback) = Self::note_dispatch_header_failure(
                                    &mut dispatch,
                                    &dispatch_attempt,
                                    &decode_err,
                                    Some((provider_resp.status, x_should_retry_false)),
                                ) {
                                    self.report_dispatch_fallback(
                                        &req,
                                        fallback,
                                        provider_resp.request_id.as_deref(),
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
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                    &retry_scope,
                                    policy == NonStreamDrivePolicy::Standard,
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
                                return Err(decode_err.with_response_diagnostics(response_id, None));
                            }
                            // Capacity acceptance precedes retry-decline headers in native eWo.
                            if account_coordinator.is_none() && crate::model::retry_scope::http_retry_decline_is_terminal(
                                provider_resp.status,
                                x_should_retry_false,
                                lingxi_llm_client::providers::anthropic::response_policy::has_overload_payload(&provider_resp.body_json),
                                crate::model::retry::retry_watchdog_from_env(),
                            ) {
                                telemetry::emit_failed(
                                    &self.analytics,
                                    &req.input.model,
                                    &request_id,
                                    Self::error_kind(&decode_err),
                                    Self::status_of(&decode_err),
                                )
                                .await;
                                return Err(decode_err.with_response_diagnostics(response_id, None));
                            }

                            // Rate-limited: resolve delay from headers.
                            let mut effective_err = decode_err.clone();
                            let mut rate_limit_message = None;
                            let mut pending_overage = None;
                            let pending_429 = if let LlmError::RateLimited { retry_after, .. } =
                                decode_err.classification_source()
                            {
                                // Task 6 (batch 5): capture the 429's OWN
                                // unified headers (errors.ts:471-516) so a
                                // terminal 429 can surface the limits copy.
                                let observation = self.record_rate_limit_from_429_for_route(
                                    &provider_resp.headers,
                                    Some(&provider_resp.body_json),
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
                                    &provider_resp.headers,
                                    prepared.prompt_cache.as_ref(),
                                    prompt_cache_header_order,
                                );
                                let delay = retry_after.or_else(|| {
                                    Self::resolve_retry_after(
                                        &provider_resp.headers,
                                        prepared.route.protocol,
                                        crate::execution::response_provider_id(
                                            &prepared.route.resolved_route.provider_id,
                                        ),
                                    )
                                });
                                if let Some(delay) = delay {
                                    telemetry::emit_rate_limited(
                                        &self.analytics,
                                        &req.input.model,
                                        u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                    )
                                    .await;
                                }
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

                            if !allow_replay {
                                if matches!(
                                    decode_err.classification_source(),
                                    LlmError::RateLimited { .. }
                                ) {
                                    self.promote_pending_429(
                                        pending_429,
                                        pending_overage,
                                    );
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

                            // 2.1.198 `V_c`/`G_c`/`s_f` + `Ygf`: an AWS-auth
                            // failure (401/403) on the Bedrock provider runs
                            // the awsAuthRefresh flow (`ZBd`) and retries,
                            // bounded at AWS_AUTH_MAX_ATTEMPTS (Ygf=2). The
                            // binary clears the memoized credential resolver
                            // (`xce()`) and lets the retry's credential
                            // resolve run ZBd; lingxi resolves credentials
                            // inside `prepare()`, so the flow runs inline
                            // before the re-prepare. Once the bound is hit
                            // the error falls through to the normal driver
                            // (Authentication ⇒ Terminal — the binary's
                            // `api_request_aws_auth_exhausted` throw).
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
                                        retry_scope.mark_account_failed(
                                            coordinator.last_failed_account_key(),
                                        );
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
                                            &provider_resp.headers,
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
                                    return Err(effective_err.with_response_diagnostics(
                                        response_id,
                                        rate_limit_message,
                                    ));
                                }
                                // Unknown transport settlement cannot be replayed under a
                                // different account or by the standalone retry driver.
                                if matches!(
                                    effective_err.classification_source(),
                                    LlmError::Transport { .. } | LlmError::TransportTimeout { .. }
                                ) {
                                    return Err(effective_err.with_response_diagnostics(
                                        response_id,
                                        rate_limit_message,
                                    ));
                                }
                            }
                            if let Some(next) = connections.advance(
                                &mut req,
                                &mut state,
                                &retry_scope,
                                &effective_err,
                            ) {
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
                                    &retry_control,
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
                                        provider_resp.status,
                                        &provider_resp.headers,
                                        retry_control.watchdog,
                                        prepared.prompt_cache.as_ref(),
                                        prompt_cache_header_order,
                                    );
                                    self.report_and_sleep_retry(
                                        &effective_err,
                                        delay,
                                        &state,
                                        &retry_control,
                                    )
                                    .await;
                                    continue;
                                }
                                DriveStep::AdjustMaxTokens(new_max) => {
                                    // Emit telemetry for the overflow adjustment.
                                    if let Some(overflow) =
                                        crate::model::overflow::parse_overflow_message(
                                            match &decode_err {
                                                LlmError::InvalidRequest { message } => message,
                                                _ => "",
                                            },
                                        )
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
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.input.model,
                                        &request_id,
                                        Self::error_kind(&decode_err),
                                        Self::status_of(&decode_err),
                                    )
                                    .await;
                                    return Err(decode_err.with_response_diagnostics(
                                        response_id,
                                        rate_limit_message,
                                    ));
                                }
                                DriveStep::Fallback { fallback_model } => {
                                    if req.execution.model_attempt.is_some() {
                                        return Err(decode_err.with_response_diagnostics(
                                            response_id,
                                            rate_limit_message,
                                        ));
                                    }
                                    // Switch to the fallback model; advance the
                                    // chain index so the next iteration's ctl
                                    // points at chain[chain_idx] (or is
                                    // exhausted → allow_fallback=false).
                                    strip_signature_blocks_for_fallback(&mut req.input.messages);
                                    let (profile, model) = self.resolve_model_fallback(
                                        chain.get(chain_idx),
                                        &fallback_model,
                                        caller_profile.as_deref(),
                                    )?;
                                    req.profile = Some(profile);
                                    req.input.model = model;
                                    req.input.model_source = None;
                                    if req.execution.selected_account.take().is_some() {
                                        req.execution.account_scope = None;
                                    }
                                    logical_reasoning.restore(&mut req);
                                    connections = super::retry::ConnectionRetry::new();
                                    if let Some(previous) = account_coordinator.as_mut() {
                                        previous.release().await;
                                    }
                                    *account_coordinator =
                                        self.make_account_coordinator(&req, &retry_scope).await?;
                                    if account_coordinator.is_some()
                                        && req.execution.api_attempts_left.is_none()
                                    {
                                        req.execution.api_attempts_left =
                                            Some(retry_scope.account_api_budget());
                                    }
                                    chain_idx += 1;
                                    // Reset the consecutive-overload counter so
                                    // the new primary model's 529 budget is fresh.
                                    state.consecutive_overloaded = 0;
                                    retry_scope.reset_model_overloads();
                                    // Rebuild retry_control with the next chain
                                    // entry (None when exhausted).
                                    let next_fallback = chain
                                        .get(chain_idx)
                                        .map(|target| target.model().to_owned());
                                    let allow_fallback = next_fallback.is_some();
                                    retry_control = resolve_retry_control_with_settings(
                                        &req.input.model,
                                        next_fallback,
                                        sub.is_subscriber,
                                        &ResolveRetryEnv::from_process_env(),
                                        self.settings_max_retries,
                                    );
                                    if allow_fallback {
                                        retry_control.allow_fallback = true;
                                    }
                                    retry_control =
                                        retry_scope.configure(&retry_control, &mut state);
                                    continue;
                                }
                                DriveStep::Terminal => {
                                    // B6-T1: the turn DIES here — promote the
                                    // 429 snapshot staged this attempt into the
                                    // live caches (the TS terminal catch handler
                                    // `extractQuotaStatusFromError`,
                                    // claudeAiLimits.ts:487). Gated on the
                                    // RateLimited discriminant so a non-429
                                    // terminal never promotes a stale slot.
                                    if matches!(
                                        decode_err.classification_source(),
                                        LlmError::RateLimited { .. }
                                    ) {
                                        self.promote_pending_429(
                                            pending_429,
                                            pending_overage,
                                        );
                                    }
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.input.model,
                                        &request_id,
                                        Self::error_kind(&decode_err),
                                        Self::status_of(&decode_err),
                                    )
                                    .await;
                                    return Err(decode_err.with_response_diagnostics(
                                        response_id,
                                        rate_limit_message,
                                    ));
                                }
                                DriveStep::RepeatedOverloaded => {
                                    // External non-sandbox threshold: surface the
                                    // repeated bit so the conversion layer produces
                                    // `OrchestratorError::RepeatedOverloaded` with the
                                    // byte-locked "Repeated 529 Overloaded errors" copy
                                    // (errors.ts:166).
                                    let repeated_err = LlmError::Overloaded { repeated: true };
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.input.model,
                                        &request_id,
                                        Self::error_kind(&repeated_err),
                                        Self::status_of(&repeated_err),
                                    )
                                    .await;
                                    return Err(
                                        repeated_err.with_response_diagnostics(response_id, None)
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Inherent provider-neutral entry points ───────────────────────────────

    /// Build a main request without clearing the session's structured-output
    /// or thinking policy, then execute every requested control together.
    pub async fn messages_create(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        self.with_account_call_snapshot(async {
            let (req, retry) = self.build_main_message_request(request, false)?;
            self.execute_non_stream_request(req, NonStreamingRequestClass::Main, retry)
                .await
        })
        .await
    }

    /// Execute a canonical request through the physical retry, watchdog and
    /// registered-attempt driver without changing its body policy.
    pub async fn execute_non_stream_request(
        &self,
        mut request: LlmRequest,
        class: NonStreamingRequestClass,
        retry: NonStreamingRetryOptions,
    ) -> Result<HistoryResponse, LlmError> {
        let resolve_configured_source = matches!(retry.fallback, FallbackPolicy::Configured)
            && request.execution.side_query_max_retries.is_none();
        let operation = async move {
            request.stream = false;
            let chain = match retry.fallback {
                FallbackPolicy::Disabled => Vec::new(),
                FallbackPolicy::Models(models) => models
                    .into_iter()
                    .map(ConfiguredFallbackTarget::Model)
                    .collect(),
                FallbackPolicy::Configured => self.configured_fallback_chain(&request),
            };
            let mut control = resolve_retry_control_with_settings(
                &request.input.model,
                chain.first().map(|target| target.model().to_owned()),
                self.effective_subscriber().is_subscriber,
                &ResolveRetryEnv::from_process_env(),
                self.settings_max_retries,
            );
            if !chain.is_empty() {
                control.allow_fallback = true;
            }
            let dispatch = match class {
                NonStreamingRequestClass::Main => DispatchHeaderState::default(),
                NonStreamingRequestClass::Auxiliary => {
                    request.execution.isolated_query = true;
                    DispatchHeaderState::AUXILIARY
                }
            };
            self.drive_non_stream_seeded_with_chain(
                request,
                control,
                retry.initial_consecutive_overloaded.unwrap_or(0),
                &chain,
                dispatch,
                NonStreamDrivePolicy::Standard,
            )
            .await
        };
        if resolve_configured_source {
            self.with_account_call_snapshot(operation).await
        } else {
            operation.await
        }
    }

    fn configured_fallback_chain(&self, request: &LlmRequest) -> Vec<ConfiguredFallbackTarget> {
        let model = &request.input.model;
        let source = self
            .client()
            .resolve_model_binding(model, request.profile.as_deref());
        if let Ok((route, binding)) = source {
            let profiles = [route.profile_name.as_str(), binding.as_str()];
            let requested_model = model
                .split_once('/')
                .filter(|(profile, _)| model != &route.request_model && profiles.contains(profile))
                .map_or(model.as_str(), |(_, model)| model);
            let model_ids = [
                route.display_model.as_str(),
                route.request_model.as_str(),
                route.pricing_model.billing_model.as_str(),
                requested_model,
            ];
            for profile in profiles {
                for model_id in model_ids {
                    if let Some(chain) = self
                        .fallback_overrides
                        .get(&format!("{profile}/{model_id}"))
                    {
                        return chain.clone();
                    }
                }
            }
            for model in [route.display_model.as_str(), requested_model] {
                if let Some(chain) = self.fallback_overrides.get(model) {
                    // Assembled routes use slash-bearing keys as qualified sources.
                    // Legacy callers can still key chains by a native model ID.
                    if !model.contains('/')
                        || chain
                            .iter()
                            .all(|target| matches!(target, ConfiguredFallbackTarget::Model(_)))
                    {
                        return chain.clone();
                    }
                }
            }
        }
        self.fallback_models
            .iter()
            .cloned()
            .map(ConfiguredFallbackTarget::Model)
            .collect()
    }

    fn resolve_model_fallback(
        &self,
        entry: Option<&ConfiguredFallbackTarget>,
        fallback_model: &str,
        caller_profile: Option<&str>,
    ) -> Result<(String, String), LlmError> {
        let client = self.client();
        if let Some(ConfiguredFallbackTarget::Profile {
            provider_id,
            profile_name,
            model,
        }) = entry
        {
            let (target, binding) = client.resolve_model_binding(model, Some(profile_name))?;
            if &binding != profile_name
                || &target.provider_id != provider_id
                || &target.request_model != model
            {
                return Err(LlmError::ModelUnavailable);
            }
            return Ok((binding, target.request_model));
        }
        let target = client
            .resolve_media_route(fallback_model, caller_profile)
            .or_else(|_| client.resolve_media_route(fallback_model, None))?
            .main;
        Ok((target.profile_name, target.request_model))
    }

    /// Use the ordinary streaming driver, admitting output only after its full
    /// assistant response completes. Native computer callers share the normal
    /// completed-response dispatcher and never act on partial stream blocks.
    pub async fn messages_create_buffered_stream(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let stream = self
            .with_account_call_snapshot(async {
                let (req, _retry) = self.build_main_message_request(request, true)?;
                self.drive_stream(req).await
            })
            .await?;
        crate::stream_accumulator::accumulate_stream_salvaging(stream)
            .await
            .map_err(|(_, error)| error)
    }
}
