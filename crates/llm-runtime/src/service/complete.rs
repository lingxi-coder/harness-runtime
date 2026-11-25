//! Non-streaming model calls and buffered stream recovery.

use crate::dispatch_header::DispatchHeaderState;
use crate::model::retry::{
    resolve_retry_control_with_settings, DriveStep, ResolveRetryEnv, RetryControl, RetryState,
};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::model::telemetry;
use crate::service::anthropic::endpoint::native_request_model_identity;
use crate::service::anthropic::recovery::strip_signature_blocks_for_fallback;
use crate::service::attempt::{boxed_model_future, new_telemetry_id};
use crate::service::conversation::ConversationLease;
use crate::service::request::{
    query_source_category, request_query_source_category, FallbackPolicy, MessagesCreateRequest,
    NonStreamingRequestClass, NonStreamingRetryOptions, QuerySourceCategory,
};
use crate::service::retry::{
    allows_account_switch, allows_automatic_replay, guard_max_tokens_adjustment,
    rate_limit_cannot_clear, reasoning_budget,
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
        chain: &'a [String],
        dispatch: DispatchHeaderState,
        policy: NonStreamDrivePolicy,
    ) -> crate::BoxFuture<'a, Result<HistoryResponse, LlmError>> {
        Box::pin(async move {
            let mut req = req;
            crate::anthropic_extra_body::capture(&mut req)?;
            let client = self.client.account_call_snapshot().await?;
            let owner = Arc::as_ptr(&self.client) as usize;
            ACCOUNT_CALL_CLIENT
                .scope((owner, client), async move {
                    let pooled = self.client().has_account_pool();
                    let scope = ModelCallRetryScope::current_or_new();
                    if pooled {
                        if let Some(error) = scope.account_replay_error() {
                            return Err(error);
                        }
                    }
                    let deadline =
                        if pooled {
                            Some(scope.account_deadline(
                                crate::model::request_timeout::non_stream_timeout()?,
                            ))
                        } else {
                            None
                        };
                    let drive = crate::model::request_trace::non_stream(boxed_model_future(|| {
                        self.drive_non_stream_inner(
                            req,
                            retry_control,
                            initial_consecutive_overloaded,
                            chain,
                            dispatch,
                            policy,
                        )
                    }));
                    if let Some(deadline) = deadline {
                        tokio::time::timeout_at(deadline, scope.run(drive))
                            .await
                            .map_err(|_| LlmError::TransportTimeout {
                                message: "logical account call deadline exceeded".into(),
                            })?
                    } else {
                        scope.run(drive).await
                    }
                })
                .await
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn drive_non_stream_inner(
        &self,
        mut req: LlmRequest,
        mut retry_control: RetryControl,
        initial_consecutive_overloaded: u8,
        chain: &[String],
        mut dispatch: DispatchHeaderState,
        policy: NonStreamDrivePolicy,
    ) -> Result<HistoryResponse, LlmError> {
        if policy == NonStreamDrivePolicy::Standard
            && req.execution.thinking_recovery_scope.is_none()
        {
            req.execution.thinking_recovery_scope = Some(self.thinking_recovery_scope());
        }
        // Native captures `nt` once for this logical query, before any request
        // awaits. Retries keep the same identity even if a reset replaces the
        // conversation's active sticky-beta object while a response is pending.
        let native_tbc_attempt = (policy == NonStreamDrivePolicy::Standard)
            .then(|| {
                req.execution
                    .thinking_recovery_scope
                    .as_ref()
                    .map(|scope| scope.native_thinking_binding_controls_attempt())
            })
            .flatten();
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
        let allow_replay = allows_automatic_replay(&req);
        let mut connections = super::retry::ConnectionRetry::new();
        telemetry::emit_started(&self.analytics, &req.input.model, &request_id, false).await;
        if let Some(query_source) = req.execution.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.input.model, query_source).await;
        }

        // Batch-5 Task 3: resolve the live subscriber state ONCE per drive call
        // (not per attempt) — RetryState persists across the retry loop, so the
        // 429/enterprise gate is stable for the whole request, matching the TS
        // granularity (the gate effectively stabilizes per request).
        let sub = self.effective_subscriber();
        let mut state = RetryState {
            consecutive_overloaded: initial_consecutive_overloaded,
            is_subscriber: sub.is_subscriber,
            is_enterprise: sub.is_enterprise,
            // Fail FAST on a rate limit that cannot clear in the backoff window
            // (a subscription plan's quota, an OpenRouter free-tier share);
            // API-key routes + Anthropic keep the parity 429-retry.
            rate_limit_terminal: rate_limit_cannot_clear(req.profile.as_deref(), &req.input.model),
            ..RetryState::default()
        };
        let retry_scope = match policy {
            NonStreamDrivePolicy::Standard => ModelCallRetryScope::current_or_new(),
            NonStreamDrivePolicy::ModelValidation => ModelCallRetryScope::default(),
        };
        let mut account_coordinator = self.make_account_coordinator(&req, &retry_scope).await?;
        if account_coordinator.is_some() && req.execution.api_attempts_left.is_none() {
            req.execution.api_attempts_left = Some(retry_scope.account_api_budget());
        }
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

        loop {
            self.select_attempt_account(account_coordinator.as_mut(), &retry_scope, &mut req)
                .await?;

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
            let model_source_units = native_request_model_identity(&req);
            self.strip_cached_foundry_tool_features(&mut req, &model_source_units);
            // Strip rejected thinking before encode so Gemini / OpenAI-compat
            // thinking models can prepare. DeepSeek / Kimi skip this.
            // prepare → inject headers → execute.
            let preparing = tokio::time::Instant::now();
            let prepare_future = async {
                let request_client = self
                    .bind_attempt_route(&mut req, policy == NonStreamDrivePolicy::Standard)
                    .await?;
                let mut prepared = request_client
                    .prepare_on(&req, self.transport.clone())
                    .await?;
                prepared.native_thinking_binding_controls_attempt = native_tbc_attempt.clone();
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
            connections.capture(
                &prepared.route.resolved_route,
                policy == NonStreamDrivePolicy::Standard && account_coordinator.is_none(),
            );
            // Pause the provider deadline while waiting for host admission.
            let prepare_elapsed = preparing.elapsed();
            let remaining = timeout.map(|timeout| timeout.saturating_sub(prepare_elapsed));
            let mut attempt = self.begin_model_attempt(&req, &prepared).await?;
            safety.inherit_if_missing(attempt.model_safety_observer());
            let dispatch_started = tokio::time::Instant::now();
            let mut admission = super::attempt::AttemptAdmission::new(self, &req);
            let mut callback_error = None;
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
            let dispatch_future = async {
                admission.check_computer_submission()?;
                prepared.before_computer_submit().await?;
                admission.authorize_quota().await?;
                let received = conversation_lease
                    .session()
                    .dispatch(call, || {
                        admission
                            .mark_dispatch(&mut attempt, &observations, &mut any_dispatched)
                            .map_err(|error| {
                                callback_error = Some(error.clone());
                                crate::execution::wire_error(error)
                            })
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
                received.collect().await.map_err(crate::upstream::error)
            };
            let resp_result = match remaining {
                Some(remaining) => {
                    crate::execution::non_stream_bound(remaining, dispatch_future).await
                }
                None => dispatch_future.await,
            };

            if callback_error.is_some() {
                if let Some(submission) = req.execution.computer_submission.as_ref() {
                    submission.not_submitted().await?;
                }
            }

            match resp_result {
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
                    if let Some(next) = connections.advance(&mut req, &mut state, &transport_err) {
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
                        let budget = prepared_output_budget_observation(&prepared, &req.input);
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
                        && allow_replay
                        && conversation_lease.session().recover(collected.response());
                    attempt.finish().await?;
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
                            self.record_rate_limit_from_headers_for_route(
                                &provider_resp.headers,
                                outgoing_client_request_id.as_deref(),
                                prepared.route.protocol,
                                crate::execution::response_provider_id(
                                    &prepared.route.resolved_route.provider_id,
                                ),
                            );
                            self.record_prompt_cache_overage_from_headers(
                                &provider_resp.headers,
                                prepared.prompt_cache.as_ref(),
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
                                Self::refuse_native_thinking_binding_controls(
                                    &mut req,
                                    &mut prepared,
                                );
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
                                let model_source_units = native_request_model_identity(&req);
                                let foundry_observation = self
                                    .record_foundry_capability_observation(
                                        &req.input.model,
                                        &model_source_units,
                                        &prepared.route.resolved_route.profile_name,
                                        prepared.route.protocol,
                                        &prepared.route.resolved_route.provider_id,
                                        provider_resp.status,
                                        &provider_resp.body_json,
                                        decode_err.provider_message(),
                                    );
                                if foundry_observation.is_some()
                                    && self.strip_cached_foundry_tool_features(
                                        &mut req,
                                        &model_source_units,
                                    )
                                {
                                    continue;
                                }
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
                                return Err(decode_err);
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
                                return Err(decode_err);
                            }

                            // Rate-limited: resolve delay from headers.
                            let mut effective_err = decode_err.clone();
                            let pending_429 = if let LlmError::RateLimited { retry_after, .. } =
                                decode_err.classification_source()
                            {
                                // Task 6 (batch 5): capture the 429's OWN
                                // unified headers (errors.ts:471-516) so a
                                // terminal 429 can surface the limits copy.
                                let pending = self.record_rate_limit_from_429_for_route(
                                    &provider_resp.headers,
                                    Some(&provider_resp.body_json),
                                    &req.input.model,
                                    outgoing_client_request_id.as_deref(),
                                    prepared.route.protocol,
                                    crate::execution::response_provider_id(
                                        &prepared.route.resolved_route.provider_id,
                                    ),
                                );
                                self.stage_prompt_cache_overage_from_429(
                                    &provider_resp.headers,
                                    prepared.prompt_cache.as_ref(),
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
                                pending
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
                                        prepared.prompt_cache.as_ref(),
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
                                return Err(decode_err);
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
                                    return Err(effective_err);
                                }
                                // Unknown transport settlement cannot be replayed under a
                                // different account or by the standalone retry driver.
                                if matches!(
                                    effective_err.classification_source(),
                                    LlmError::Transport { .. } | LlmError::TransportTimeout { .. }
                                ) {
                                    return Err(effective_err);
                                }
                            }
                            if let Some(next) =
                                connections.advance(&mut req, &mut state, &effective_err)
                            {
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
                                    return Err(decode_err);
                                }
                                DriveStep::Fallback { fallback_model } => {
                                    if req.execution.model_attempt.is_some() {
                                        return Err(decode_err);
                                    }
                                    // Switch to the fallback model; advance the
                                    // chain index so the next iteration's ctl
                                    // points at chain[chain_idx] (or is
                                    // exhausted → allow_fallback=false).
                                    strip_signature_blocks_for_fallback(&mut req.input.messages);
                                    req.input.model = fallback_model;
                                    chain_idx += 1;
                                    // Reset the consecutive-overload counter so
                                    // the new primary model's 529 budget is fresh.
                                    state.consecutive_overloaded = 0;
                                    retry_scope.reset_model_overloads();
                                    // Rebuild retry_control with the next chain
                                    // entry (None when exhausted).
                                    let next_fallback = chain.get(chain_idx).cloned();
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
                                            prepared.prompt_cache.as_ref(),
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
                                    return Err(decode_err);
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
                                    return Err(repeated_err);
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
        let (req, retry) = self.build_main_message_request(request, false)?;
        self.execute_non_stream_request(req, NonStreamingRequestClass::Main, retry)
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
        request.stream = false;
        let model = &request.input.model;
        let display_model = self
            .alias_to_display
            .get(model)
            .map_or(model.as_str(), String::as_str);
        let chain = match retry.fallback {
            FallbackPolicy::Disabled => Vec::new(),
            FallbackPolicy::Models(models) => models,
            FallbackPolicy::Configured => self
                .fallback_overrides
                .get(display_model)
                .cloned()
                .unwrap_or_else(|| self.fallback_models.clone()),
        };
        let mut control = resolve_retry_control_with_settings(
            model,
            chain.first().cloned(),
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
    }

    /// Use the ordinary streaming driver, admitting output only after its full
    /// assistant response completes. Native computer callers share the normal
    /// completed-response dispatcher and never act on partial stream blocks.
    pub async fn messages_create_buffered_stream(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let (req, _retry) = self.build_main_message_request(request, true)?;
        let stream = self.drive_stream(req).await?;
        crate::stream_accumulator::accumulate_stream_salvaging(stream)
            .await
            .map_err(|(_, error)| error)
    }
}
