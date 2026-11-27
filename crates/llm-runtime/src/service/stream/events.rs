//! Event delivery and settlement after the stream connects.

use super::StreamState;
use crate::model::telemetry;
use crate::service::usage::{
    attach_frozen_stream_quote, frozen_server_fallback_quote, frozen_stream_quote,
    server_fallback_quote_candidate, ServerFallbackQuoteProjection,
};
use crate::service::ApiService;
use crate::{HistoryEvent, LlmError};

pub(super) async fn next(
    mut s: StreamState,
) -> Option<(Result<HistoryEvent, LlmError>, StreamState)> {
    let result=async move {
    loop {
        if let Some(mut event) = s.queue.pop_front() {
            let usage = match &mut event {
                HistoryEvent::MessageStart { response } | HistoryEvent::Completed { response } => Some(&mut response.usage),
                HistoryEvent::MessageDelta { usage, .. } => usage.as_mut(),
                _ => None,
            };
            if let Some((prefix, usage)) = s.context_usage_prefix.as_ref().zip(usage) {
                if let Err(error) = prefix.attach(usage) {
                    s.finished = true;
                    return Some((Err(error), s));
                }
            }
            if let HistoryEvent::MessageStart { response }
            | HistoryEvent::Completed { response } = &mut event
            {
                response.usage.account_usage = s
                    .account_coordinator
                    .as_ref()
                    .and_then(|coordinator| coordinator.usage_attribution());
                if let Err(error) =
                    response.set_per_turn_effort(s.per_turn_effort.as_ref())
                        .and_then(|_| response.set_diagnostics(&s.diagnostics))
                {
                    s.finished = true;
                    return Some((Err(error), s));
                }
            }
            if s.mark_context_1m_latch_on_message_start
                && matches!(&event, HistoryEvent::MessageStart { .. })
            {
                if let Some((scope, model_key)) =
                    s.context_1m_latch.as_ref()
                {
                    scope.mark_context_1m_refused_for(model_key.clone());
                }
                s.mark_context_1m_latch_on_message_start = false;
            }
            // Emit succeed telemetry on the terminal event
            // (MessageStop or Completed) — once, guarded by `done`.
            let is_terminal = matches!(
                event,
                HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
            );
            if is_terminal && !s.done {
                if let Err(error) = s.attempt.finish().await {
                    s.finished = true;
                    s.queue.clear();
                    return Some((Err(error), s));
                }
                s.done = true;
                let elapsed_ms =
                    u64::try_from(s.started.elapsed().as_millis())
                        .unwrap_or(u64::MAX);
                telemetry::emit_succeeded(
                    &s.analytics,
                    &s.model,
                    &s.request_id,
                    elapsed_ms,
                    200,
                )
                .await;
            }
            return Some((Ok(event), s));
        }
        if let Some(error) = s.pending_service_error.take() {
            let error = s.attempt.finish().await.err().unwrap_or(error);
            s.finished = true;
            if !s.done {
                s.done = true;
                telemetry::emit_failed(
                    &s.analytics,
                    &s.model,
                    &s.request_id,
                    ApiService::error_kind(&error),
                    ApiService::status_of(&error),
                )
                .await;
            }
            return Some((Err(error), s));
        }
        if let Some(error) = s.decoder.take_error() {
            let error = s.attempt.finish().await.err().unwrap_or(error);
            s.finished = true;
            if !s.done {
                s.done = true;
                telemetry::emit_failed(
                    &s.analytics,
                    &s.model,
                    &s.request_id,
                    ApiService::error_kind(&error),
                    ApiService::status_of(&error),
                )
                .await;
            }
            return Some((Err(error), s));
        }
        if s.finished {
            if let Err(error) = s.attempt.finish().await {
                return Some((Err(error), s));
            }
            if let Some(coordinator) = s.account_coordinator.as_mut() {
                coordinator.release().await;
            }
            return None;
        }
        // Watchdog: bound the blocking frame read by the
        // configured idle timeout (reset per event). On
        // elapse, abort the stream with a detectable
        // idle-timeout error (binary
        // `tengu_streaming_watchdog_retry` surface).
        let seed_has_usage = s.server_fallback_lane.is_none()
            && s.seed.as_ref().is_some_and(|seed| matches!(seed, Ok(Some(batch)) if batch.usage.usage.is_some()));
        let frame = match s.seed.take() {
            Some(seeded) => seeded,
            None => crate::execution::classifier_attempt_bound(s.classifier_attempt_deadline, async { match s.idle_timeout {
                Some(timeout) => tokio::time::timeout(
                    timeout,
                    crate::execution::next_batch_with_capture(s.frames.as_mut().expect("live provider stream"), s.native_stream_capture.as_ref()),
                )
                .await
                .unwrap_or_else(|_elapsed| {
                    Err(crate::model::stream_watchdog::idle_timeout_error(
                        timeout,
                    ))
                }),
                None => crate::execution::next_batch_with_capture(s.frames.as_mut().expect("live provider stream"), s.native_stream_capture.as_ref()).await,
            }}).await,
        };
        match frame {
            Ok(Some(frame)) => {
                s.safety.batch(&frame);
                let terminal_frame = frame.events.iter().any(|event| {
                    matches!(event, Ok(lingxi_llm_client::protocol::StreamEvent::End { .. }))
                });
                let terminal_fallback_boundary = frame.events.iter().any(|event| {
                    let Ok(lingxi_llm_client::protocol::StreamEvent::NativeControl {
                        protocol: lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
                        control,
                    }) = event else {
                        return false;
                    };
                    matches!(
                        control.decode::<lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl>(),
                        Ok(lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl::Boundary {
                            stop_reason: Some(_),
                            ..
                        })
                    )
                });
                let terminal_quote_frame =
                    terminal_frame || terminal_fallback_boundary;
                let iteration_array_cleared = frame.events.iter().any(|event| {
                    let Ok(lingxi_llm_client::protocol::StreamEvent::NativeControl {
                        protocol: lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
                        control,
                    }) = event else {
                        return false;
                    };
                    let Ok(lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl::Boundary {
                        iterations,
                        iterations_present: true,
                        ..
                    }) = control.decode::<lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl>() else {
                        return false;
                    };
                    iterations.served_fallback_model.is_none()
                });
                if iteration_array_cleared
                    && s.server_fallback_quote_candidate_seen
                {
                    // Keep the provider-facing usage snapshot clean from
                    // this Boundary onward; the stop-reason-bearing
                    // Boundary decides which quote branch won.
                    if let Err(error) =
                        s.decoder.clear_server_fallback_cost_quote()
                    {
                        s.pending_service_error = Some(error);
                        s.finished = true;
                        continue;
                    }
                    s.server_fallback_quote_metadata = None;
                }
                let mut quote_observation = None;
                let quote = if let Some(lane) =
                    s.server_fallback_lane.as_ref()
                {
                    let fallback = s.frames.as_ref().expect("live provider stream").anthropic_fallback();
                    let native_branch = fallback
                        .and_then(|facts| facts.iterations.as_ref())
                        .is_some_and(|iterations| {
                            iterations.served_fallback_model.is_some()
                        });
                    if terminal_quote_frame
                        && !s.server_fallback_quote_finalized
                        && native_branch
                    {
                        // A typed terminal Boundary owns the final stop
                        // reason and iteration array. Do not let an
                        // earlier provisional observation lock the quote.
                        if let Some(ServerFallbackQuoteProjection {
                            estimate,
                            metadata,
                            summary_model,
                        }) = frozen_server_fallback_quote(
                            s.pricing.as_ref(),
                            &s.pricing_model,
                            &lane.model,
                            fallback,
                            &frame.inference,
                        ) {
                            s.server_fallback_quote_finalized = true;
                            s.server_fallback_quote_estimate =
                                estimate.clone();
                            quote_observation =
                                Some(HistoryEvent::CostQuoteObserved {
                                    estimate: estimate.clone(),
                                    native_server_fallback: true,
                                    summary_model,
                                });
                            if let Err(error) = s
                                .decoder
                                .observe_server_fallback_cost_quote(
                                    metadata.clone(),
                                )
                            {
                                s.pending_service_error = Some(error);
                                s.finished = true;
                                continue;
                            }
                            s.server_fallback_quote_metadata =
                                Some(metadata);
                            estimate
                        } else {
                            None
                        }
                    } else if terminal_frame
                        && !s.server_fallback_quote_finalized
                        && s.server_fallback_quote_candidate_seen
                    {
                        // An explicit final `iterations: []` revokes
                        // the earlier cNe branch. Clear its reserved
                        // marker and use the ordinary aggregate quote.
                        let estimate = frozen_stream_quote(
                            s.pricing.as_ref(),
                            &s.pricing_model,
                            &frame.usage,
                            &frame.inference,
                        );
                        s.server_fallback_quote_finalized = true;
                        s.server_fallback_quote_estimate = estimate.clone();
                        s.server_fallback_quote_metadata = None;
                        if let Err(error) =
                            s.decoder.clear_server_fallback_cost_quote()
                        {
                            s.pending_service_error = Some(error);
                            s.finished = true;
                            continue;
                        }
                        quote_observation =
                            Some(HistoryEvent::CostQuoteObserved {
                                estimate: estimate.clone(),
                                native_server_fallback: false,
                                summary_model: None,
                            });
                        estimate
                    } else if terminal_quote_frame
                        && s.server_fallback_quote_finalized
                    {
                        s.server_fallback_quote_estimate.clone()
                    } else if !terminal_quote_frame && !native_branch {
                        // Keep an earlier candidate until terminal facts
                        // confirm or revoke it. A partial usage boundary
                        // can omit the iteration array entirely.
                        None
                    } else if !terminal_quote_frame && native_branch {
                        if let Some(ServerFallbackQuoteProjection {
                            estimate: _,
                            metadata,
                            summary_model,
                        }) = server_fallback_quote_candidate(
                            &lane.model,
                            fallback,
                        ) {
                            s.server_fallback_quote_candidate_seen = true;
                            if s.server_fallback_quote_metadata.as_ref()
                                != Some(&metadata)
                            {
                                quote_observation =
                                    Some(HistoryEvent::CostQuoteObserved {
                                        estimate: None,
                                        native_server_fallback: true,
                                        summary_model,
                                    });
                                if let Err(error) = s
                                    .decoder
                                    .observe_server_fallback_cost_quote(
                                        metadata.clone(),
                                    )
                                {
                                    s.pending_service_error = Some(error);
                                    s.finished = true;
                                    continue;
                                }
                                s.server_fallback_quote_metadata =
                                    Some(metadata);
                            }
                        }
                        None
                    } else {
                        frozen_stream_quote(
                            s.pricing.as_ref(),
                            &s.pricing_model,
                            &frame.usage,
                            &frame.inference,
                        )
                    }
                } else {
                    frozen_stream_quote(
                        s.pricing.as_ref(),
                        &s.pricing_model,
                        &frame.usage,
                        &frame.inference,
                    )
                };
                if let Err(error) =
                    s.decoder.observe_model_metadata(s.frames.as_ref().expect("live provider stream"))
                {
                    s.pending_service_error = Some(error);
                    s.finished = true;
                    continue;
                }
                match s.decoder.project_batch(frame) {
                    Ok(mut events) => {
                        attach_frozen_stream_quote(
                            &mut events,
                            quote.as_ref(),
                        );
                        if let Some(observation) = quote_observation {
                            // The physical response may still carry a
                            // billable quote if the host session later
                            // declines its controller-facing hop.
                            let terminal = events.iter().position(|event| {
                                matches!(
                                    event,
                                    HistoryEvent::ServerFallback { .. }
                                        | HistoryEvent::MessageDelta { .. }
                                        | HistoryEvent::MessageStop
                                        | HistoryEvent::Completed { .. }
                                )
                            });
                            events.insert(
                                terminal.unwrap_or(events.len()),
                                observation,
                            );
                        }
                        if let Some((mut usage, completeness)) =
                            s.decoder.observed_usage()
                        {
                            usage.cost_estimate = quote.clone();
                            if let Some(metadata) =
                                &s.server_fallback_quote_metadata
                            {
                                if let Err(error) = crate::history_projection::attach_server_fallback_cost_quote(
                                    &mut usage.provider_metadata,
                                    metadata.clone(),
                                ) {
                                    s.pending_service_error = Some(error);
                                    s.finished = true;
                                    continue;
                                }
                            }
                            if !seed_has_usage {
                                s.attempt.observe(&usage, completeness);
                            }
                        } else {
                            s.attempt.observe_events(&events);
                        }
                        s.queue.extend(events);
                    }
                    Err(e) => {
                        s.native_thinking_resumption_refused.observe(&e);
                        s.safety.error(&e);
                        if let Some((mut usage, completeness)) =
                            s.decoder.observed_usage()
                        {
                            usage.cost_estimate = quote.clone();
                            if let Some(metadata) =
                                &s.server_fallback_quote_metadata
                            {
                                if let Err(error) = crate::history_projection::attach_server_fallback_cost_quote(
                                    &mut usage.provider_metadata,
                                    metadata.clone(),
                                ) {
                                    s.pending_service_error = Some(error);
                                    s.finished = true;
                                    continue;
                                }
                            }
                            if !seed_has_usage {
                                s.attempt.observe(&usage, completeness);
                            }
                        }
                        let e = s.attempt.finish().await.err().unwrap_or(e);
                        s.finished = true;
                        if !s.done {
                            s.done = true;
                            telemetry::emit_failed(
                                &s.analytics,
                                &s.model,
                                &s.request_id,
                                ApiService::error_kind(&e),
                                ApiService::status_of(&e),
                            )
                            .await;
                        }
                        if let Some(observation) = quote_observation {
                            s.pending_service_error = Some(e);
                            return Some((Ok(observation), s));
                        }
                        return Some((Err(e), s));
                    }
                }
            }
            Ok(None) => {
                s.finished = true;
                // EOF may follow an End frame that already
                // selected native per-iteration accounting.
                // Preserve that quote (including None for an
                // incomplete native quote) instead of replacing
                // it with the dispatched-model aggregate price.
                let quote = if s.server_fallback_quote_finalized
                    || s.server_fallback_quote_candidate_seen
                    || s.server_fallback_quote_metadata.is_some()
                {
                    s.server_fallback_quote_estimate.clone()
                } else {
                    frozen_stream_quote(
                        s.pricing.as_ref(),
                        &s.pricing_model,
                        &s.frames.as_ref().expect("live provider stream").usage_report(),
                        &s.frames.as_ref().expect("live provider stream").inference_report(),
                    )
                };
                match s.decoder.finish() {
                    Ok(mut events) => {
                        attach_frozen_stream_quote(
                            &mut events,
                            quote.as_ref(),
                        );
                        if let Some((mut usage, completeness)) =
                            s.decoder.observed_usage()
                        {
                            usage.cost_estimate = quote.clone();
                            if let Some(metadata) =
                                &s.server_fallback_quote_metadata
                            {
                                if let Err(error) = crate::history_projection::attach_server_fallback_cost_quote(
                                    &mut usage.provider_metadata,
                                    metadata.clone(),
                                ) {
                                    s.pending_service_error = Some(error);
                                    s.finished = true;
                                    continue;
                                }
                            }
                            s.attempt.observe(&usage, completeness);
                        } else {
                            s.attempt.observe_events(&events);
                        }
                        // A bare EOF would lose the admitted route in a consumer's
                        // synthesized truncation error. Keep the transport-close
                        // classification so already visible text can be salvaged.
                        if !s.allow_replay && !s.done && !events.iter().any(|event| {
                            matches!(event, HistoryEvent::MessageStop | HistoryEvent::Completed { .. })
                        }) {
                            s.pending_service_error = Some(LlmError::Transport {
                                message: "stream ended without message_stop or completed event".into(),
                            });
                        }
                        s.queue.extend(events);
                    }
                    Err(e) => {
                        s.native_thinking_resumption_refused.observe(&e);
                        s.safety.error(&e);
                        if let Some((mut usage, completeness)) =
                            s.decoder.observed_usage()
                        {
                            usage.cost_estimate = quote.clone();
                            if let Some(metadata) =
                                &s.server_fallback_quote_metadata
                            {
                                if let Err(error) = crate::history_projection::attach_server_fallback_cost_quote(
                                    &mut usage.provider_metadata,
                                    metadata.clone(),
                                ) {
                                    s.pending_service_error = Some(error);
                                    s.finished = true;
                                    continue;
                                }
                            }
                            s.attempt.observe(&usage, completeness);
                        }
                        let e = s.attempt.finish().await.err().unwrap_or(e);
                        if !s.done {
                            s.done = true;
                            telemetry::emit_failed(
                                &s.analytics,
                                &s.model,
                                &s.request_id,
                                ApiService::error_kind(&e),
                                ApiService::status_of(&e),
                            )
                            .await;
                        }
                        return Some((Err(e), s));
                    }
                }
            }
            Err(e) => {
                s.native_thinking_resumption_refused.observe(&e);
                s.safety.error(&e);
                if let Some((usage, completeness)) =
                    s.decoder.observed_usage()
                {
                    s.attempt.observe(&usage, completeness);
                }
                let e = s.attempt.finish().await.err().unwrap_or(e);
                s.finished = true;
                if !s.done {
                    s.done = true;
                    telemetry::emit_failed(
                        &s.analytics,
                        &s.model,
                        &s.request_id,
                        ApiService::error_kind(&e),
                        ApiService::status_of(&e),
                    )
                    .await;
                }
                return Some((Err(e), s));
            }
        }
    }
    }.await;
    match result {
        Some((item, mut state)) => {
            if state.finished {
                state.frames.take();
                state.side_ws_lease.take();
            }
            let item = item.map_err(|error| {
                error.with_response_diagnostics(state.diagnostics.provider_request_id(), None)
                    .with_route_replay(state.allow_replay)
            });
            if let Some(coordinator) = state.account_coordinator.as_mut() {
                match &item {
                    Ok(event) => {
                        if matches!(
                            event,
                            HistoryEvent::ContentBlockStart { .. }
                                | HistoryEvent::ContentBlockDelta { .. }
                                | HistoryEvent::WebSearch { .. }
                        ) {
                            state.account_retry_scope.record_account_output();
                        }
                        if matches!(
                            event,
                            HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
                        ) {
                            coordinator.succeeded().await;
                        }
                    }
                    Err(error) => {
                        state.account_retry_scope.record_account_stream_error(error);
                        let authentication_current = if matches!(
                            error.classification_source(),
                            LlmError::Authentication { .. } | LlmError::OAuthRefreshDead
                        ) {
                            match state.account_authentication.as_ref() {
                                Some((runtime, model, profile, rejected, material)) => {
                                    let version_current = runtime
                                        .current_account_credential_revision(model, profile)
                                        .await
                                        .is_ok_and(|current| current == *rejected);
                                    version_current
                                        && match material {
                                            Some(material) => runtime
                                                .rejected_account_credential_is_current(
                                                    profile, material,
                                                )
                                                .await
                                                .unwrap_or(false),
                                            None => true,
                                        }
                                }
                                None => true,
                            }
                        } else {
                            true
                        };
                        coordinator.set_rejected_credential_revision(
                            state
                                .account_authentication
                                .as_ref()
                                .and_then(|(_, _, _, revision, _)| *revision),
                        );
                        let recorded = if authentication_current {
                            if state.account_derived_token
                                && matches!(
                                    error.classification_source(),
                                    LlmError::Authentication { .. }
                                )
                            {
                                coordinator.derived_token_rejected().await
                            } else {
                                coordinator
                                    .failed_with_retry_after(error, state.account_retry_after)
                                    .await
                            }
                        } else {
                            false
                        };
                        if recorded {
                            state
                                .account_retry_scope
                                .mark_account_failed(coordinator.last_failed_account_key());
                        } else {
                            coordinator.release().await;
                        }
                    }
                }
            }
            Some((item, state))
        }
        None => None,
    }
}
