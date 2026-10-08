//! `turn.step` over the main loop's complete-response provider path.
//!
//! The native bottom emits a non-streaming assistant record as one opaque
//! engine chunk. We retain the existing prompt-too-long recovery and cost
//! preflight path, then let Mods wrap that record or stream a replacement.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use hooks::attachment::HookPublicationGuard;
use lingxi_core::types::{ConversationMessage, MessageId};
use llm_runtime::{HistoryEvent, HistoryResponse};
use serde_json::{Value, json};

use super::{PtlCallOutcome, call_api_with_ptl_recovery};
use crate::conversation::{ConversationOrchestrator, OutgoingHistoryRewriter};
use crate::error::OrchestratorError;
use crate::mod_turn_step::{TurnStepDecoder, TurnStepEncoder};

#[derive(Clone)]
pub(super) struct Request {
    pub system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    pub skip_global_cache_for_system_prompt: bool,
    pub model: String,
    pub profile: Option<String>,
    pub history: Vec<ConversationMessage>,
    pub rewriter: Option<Arc<dyn OutgoingHistoryRewriter>>,
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    pub max_tokens: Option<u32>,
    pub deferred: Option<ConversationMessage>,
    pub date_change: Option<ConversationMessage>,
    pub reminders: Vec<ConversationMessage>,
    pub guarded_async_hook_reminders: Vec<(MessageId, Arc<dyn HookPublicationGuard>)>,
    pub context_announcements: crate::conversation::PreparedContextAnnouncements,
    pub cost_scope: Option<cost::CostSessionScope>,
    pub turn_id: String,
    pub index: u32,
    pub effort: Option<String>,
    pub api_success_message_count: u32,
    pub api_success_message_tokens: u64,
}

struct PhysicalObservation {
    system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    cost_scope: Option<cost::CostSessionScope>,
    api_success_message_count: u32,
    api_success_message_tokens: u64,
}

impl From<&Request> for PhysicalObservation {
    fn from(request: &Request) -> Self {
        Self {
            system: request.system.clone(),
            tools: request.tools.clone(),
            cost_scope: request.cost_scope.clone(),
            api_success_message_count: request.api_success_message_count,
            api_success_message_tokens: request.api_success_message_tokens,
        }
    }
}

pub(super) struct Answer {
    pub outcome: PtlCallOutcome,
    pub preceding_responses: Vec<HistoryResponse>,
    pub model: String,
    pub profile: Option<String>,
    pub requested_model: bool,
    pub physical_responses: Vec<PhysicalStepResponse>,
    pub response_request_history_source: Option<RequestHistorySource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestHistorySource {
    /// The response id matches a successful provider response and its PTL-
    /// resolved request snapshot was retained on `PhysicalStepResponse`.
    PhysicalRequest,
    /// The Mod produced a synthetic response without a matching successful
    /// provider call; use the exact input `w` supplied to `turn.step`.
    TurnStepInputForSyntheticResponse,
}

pub(super) struct PhysicalStepResponse {
    pub response: HistoryResponse,
    pub request_history: Vec<ConversationMessage>,
    pub model: String,
    pub profile: Option<String>,
    pub duration: Duration,
    pub retries: u32,
    pub request_id: Option<String>,
}

impl PhysicalStepResponse {
    async fn capture(
        orch: &ConversationOrchestrator,
        response: HistoryResponse,
        request_history: Vec<ConversationMessage>,
        model: String,
        profile: Option<String>,
        duration: Duration,
        params: &PhysicalObservation,
    ) -> Self {
        let retries = orch.api.last_retry_count();
        let request_id = orch.api.last_request_id();
        let cache_read = response.usage.counts().cache_read_tokens;
        let cache_create = response.usage.counts().cache_write_tokens;
        // Submit as soon as the physical request succeeds. If a later Mod
        // throws or the cancelable turn drops its future, the cost supervisor
        // still owns this observed charge.
        let cost_receipt = orch.model_runtime.cost_tracker.as_ref().map(|_| {
            let native_quote =
                crate::cost_wiring::has_native_fallback_quote(&response.provider_metadata);
            let (quoted_model, pricing) =
                crate::cost_wiring::response_pricing(response.cost.as_ref(), native_quote);
            let billing_model = if native_quote {
                crate::cost_wiring::native_fallback_cost_model(&response.provider_metadata)
                    .unwrap_or(&model)
            } else {
                &model
            };
            let model_ref = quoted_model.unwrap_or_else(|| {
                crate::cost_wiring::model_ref_from_string(billing_model, profile.as_deref())
            });
            let measurement = if response.usage.report.usage.is_some() {
                cost::CostResponseMeasurement::Observed
            } else {
                cost::CostResponseMeasurement::Missing
            };
            let receipt = params
                .cost_scope
                .as_ref()
                .expect("a wired cost tracker captured its scope before provider dispatch")
                .submit_model_response_with_pricing(
                    cost::CostModelResponse {
                        model_ref: model_ref.clone(),
                        usage: crate::cost_wiring::llm_usage_to_cost_usage(&response.usage),
                        duration,
                        retries,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_create,
                        is_batch_request: false,
                        bus: orch.model_runtime.analytics_bus.clone(),
                    },
                    pricing,
                    measurement,
                );
            (receipt, model_ref)
        });
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        orch.model_runtime
            .prompt_cache_ledger
            .lock()
            .await
            .ledger
            .record(cost::prompt_cache_ledger::RequestFacts {
                at_ms: now_ms,
                input_tokens: response.usage.counts().input_tokens,
                cache_read_tokens: cache_read,
                cache_creation_tokens: cache_create,
                ttl: cost::prompt_cache_ledger::CacheTtl::FiveMinutes,
            });
        orch.record_mod_turn_provider_usage(&response.usage, &response.model);
        orch.record_response_input_tokens(&response.usage);
        orch.record_inline_prompt_tools_after_success(&params.tools)
            .await;
        let display_system = params.system.as_ref().map(
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::display_text,
        );
        orch.save_cache_safe_params(display_system.as_deref(), &model, &params.tools)
            .await;
        orch.emit_rate_limit_if_changed().await;
        orch.emit_raw_utilization_if_changed().await;
        if let Some((receipt, model_ref)) = cost_receipt {
            let settlement = receipt.settle().await;
            let observed_cost = settlement.observed_nano_usd();
            orch.model_runtime
                .api_calls_recorded
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Err(error) = settlement.persistence_result() {
                orch.note_cost_settlement_failure(error).await;
            }
            if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
                let dur_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                cost::emit_api_success(
                    bus,
                    &cost::ApiSuccessFields {
                        model: model.clone(),
                        input_tokens: response.usage.counts().input_tokens,
                        output_tokens: response
                            .usage
                            .counts()
                            .output_tokens
                            .saturating_sub(response.usage.counts().reasoning_tokens),
                        cached_input_tokens: cache_read,
                        uncached_input_tokens: cache_create,
                        duration_ms: dur_ms,
                        duration_ms_including_retries: dur_ms,
                        attempt: retries + 1,
                        cost_nano_usd: observed_cost,
                        provider: crate::cost_wiring::provider_tag(&model_ref.provider),
                        stop_reason: response.stop_reason.clone(),
                        request_id: request_id.clone(),
                        message_count: params.api_success_message_count,
                        message_tokens: params.api_success_message_tokens,
                        did_fall_back_to_non_streaming: false,
                        is_non_interactive_session: !orch.prompt_is_interactive(),
                        print: orch.config.print,
                        is_tty: orch.config.is_tty,
                        query_source: crate::config::sanitize_query_source(
                            &orch.config.query_source,
                        )
                        .to_string(),
                        permission_mode: if orch.session.lock().await.plan_mode {
                            "plan"
                        } else {
                            "default"
                        }
                        .to_string(),
                        ttft_ms: None,
                        fast_mode: response.usage.inference.service_tier
                            == Some(llm_runtime::services::sdk::protocol::ServiceTier::Fast),
                        time_since_last_api_call_ms: orch.record_api_call_gap_ms(),
                    },
                )
                .await;
            }
        }
        Self {
            response,
            request_history,
            model,
            profile,
            duration,
            retries,
            request_id,
        }
    }
}

pub(super) async fn dispatch(
    orch: &ConversationOrchestrator,
    host: Arc<hooks::mods::ModHost>,
    mut params: Request,
) -> Result<Answer, OrchestratorError> {
    let input = json!({
        "turnId":params.turn_id,
        "index":params.index,
        "model":params.model,
        "messageCount":params.history.len(),
    });
    let mut input = input;
    if let Some(effort) = params.effort.as_ref() {
        input["effort"] = json!(effort);
    }
    let wire = Arc::new(Mutex::new(TurnStepEncoder::default()));
    let decoder = Arc::new(Mutex::new(TurnStepDecoder::new(&params.model)));
    let emitted = Arc::new(Mutex::new(Vec::<HistoryEvent>::new()));
    let effective = Arc::new(Mutex::new(None::<(String, Option<String>)>));
    let unrecovered = Arc::new(Mutex::new(None::<PtlCallOutcome>));
    let call_error = Arc::new(Mutex::new(None::<OrchestratorError>));
    let requested_model = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let physical_responses = Arc::new(Mutex::new(Vec::<PhysicalStepResponse>::new()));
    let source_params = params.clone();
    let source_wire = wire.clone();
    let source_effective = effective.clone();
    let source_unrecovered = unrecovered.clone();
    let source_error = call_error.clone();
    let source_requested = requested_model.clone();
    let source_physical = physical_responses.clone();
    let source = move |forwarded: Value| {
        let mut params = source_params.clone();
        let wire = source_wire.clone();
        let effective = source_effective.clone();
        let unrecovered = source_unrecovered.clone();
        let call_error = source_error.clone();
        let requested_model = source_requested.clone();
        let physical_responses = source_physical.clone();
        async move {
            let requested = forwarded["model"].as_str().unwrap_or_default().to_owned();
            let resolved = orch
                .api
                .resolve_media_route(&requested, params.profile.as_deref())
                .or_else(|_| orch.api.resolve_media_route(&requested, None))
                .ok();
            let resolved_model = resolved.as_ref().map_or(requested.as_str(), |route| {
                route.main.request_model.as_str()
            });
            let denied = if let Some(reader) = orch.mod_settings_reader.as_ref() {
                reader.model_allowed(resolved_model).await? == Some(false)
            } else {
                false
            };
            let model = if denied {
                tracing::warn!(
                    requested,
                    "turn.step batched model rewrite denied by policy"
                );
                params.model.clone()
            } else {
                requested
            };
            let profile = if denied {
                params.profile.clone()
            } else {
                resolved
                    .map(|route| route.main.profile_name)
                    .or(params.profile.clone())
            };
            *effective.lock().unwrap() = Some((model.clone(), profile.clone()));
            requested_model.store(true, std::sync::atomic::Ordering::Release);
            let effort_override = forwarded["effort"]
                .as_str()
                .filter(|effort| Some(*effort) != params.effort.as_deref());
            let observation = PhysicalObservation::from(&params);
            let started = Instant::now();
            let fallback_context =
                match lingxi_core::host::refusal_driver::current_fallback_target() {
                    Some(context) => context,
                    None => crate::query_model::fallback_target_context(orch, &model, None).await,
                };
            crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                model: model.clone(),
                profile: profile.clone(),
            });
            let outcome = lingxi_core::host::refusal_driver::scope_fallback_target(
                fallback_context,
                crate::provider_adapter::with_mod_turn_step_effort(
                    effort_override,
                    call_api_with_ptl_recovery(
                        orch,
                        params.system.as_ref(),
                        params.skip_global_cache_for_system_prompt,
                        &model,
                        profile.as_deref(),
                        params.history,
                        params.rewriter,
                        params.tools,
                        params.max_tokens,
                        params.deferred,
                        params.date_change,
                        params.reminders.clone(),
                        &mut params.guarded_async_hook_reminders,
                        &params.context_announcements,
                        params.cost_scope.as_ref(),
                    ),
                ),
            )
            .await;
            let (response, request_history) = match outcome {
                Ok(PtlCallOutcome::Response {
                    response,
                    request_history,
                }) => (response, request_history),
                Ok(other) => {
                    *unrecovered.lock().unwrap() = Some(other);
                    return Err(hooks::mods::ModError::Hook(
                        "turn.step provider request ended at a context limit".into(),
                    ));
                }
                Err(error) => {
                    let message = error.to_string();
                    *call_error.lock().unwrap() = Some(error);
                    return Err(hooks::mods::ModError::Hook(message));
                }
            };
            let physical = PhysicalStepResponse::capture(
                orch,
                response.as_ref().clone(),
                request_history,
                model,
                profile,
                started.elapsed(),
                &observation,
            )
            .await;
            physical_responses.lock().unwrap().push(physical);
            let held = wire
                .lock()
                .unwrap()
                .push(HistoryEvent::Completed { response })
                .clone();
            let chunk = hooks::mods::ModUtf16ValueProjection::from_core_projection(held.chunk)?;
            let reference = held.reference;
            let result_wire = wire.clone();
            let turn_id = forwarded["turnId"].as_str().unwrap_or_default().to_owned();
            let index = forwarded["index"].as_u64().unwrap_or_default() as u32;
            Ok(hooks::mods::ModStreamSource::new(
                futures::stream::iter(vec![Ok(chunk)]),
                move || {
                    let result = result_wire.lock().unwrap().result_for_references(
                        &turn_id,
                        index,
                        &[reference],
                    );
                    hooks::mods::ModUtf16ValueProjection::from_core_projection(result)
                },
            ))
        }
    };
    let output_wire = wire.clone();
    let output_decoder = decoder.clone();
    let output_events = emitted.clone();
    let on_chunk = move |chunk: hooks::mods::ModUtf16ValueProjection| {
        let wire = output_wire.clone();
        let decoder = output_decoder.clone();
        let emitted = output_events.clone();
        async move {
            let chunk = chunk.into_core_projection()?;
            let events = {
                let wire = wire.lock().unwrap();
                decoder.lock().unwrap().consume(&chunk, &wire)
            };
            emitted.lock().unwrap().extend(events);
            Ok(())
        }
    };
    let dispatch = host
        .dispatch_turn_step_stream(input, source, on_chunk)
        .await;
    let (model, profile) = effective
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| (params.model.clone(), params.profile.clone()));
    if let Err(error) = dispatch {
        if let Some(outcome) = unrecovered.lock().unwrap().take() {
            return Ok(Answer {
                outcome,
                preceding_responses: Vec::new(),
                model,
                profile,
                requested_model: true,
                physical_responses: std::mem::take(&mut *physical_responses.lock().unwrap()),
                response_request_history_source: None,
            });
        }
        if let Some(error) = call_error.lock().unwrap().take() {
            return Err(error);
        }
        if !requested_model.load(std::sync::atomic::Ordering::Acquire) {
            let observation = PhysicalObservation::from(&params);
            let started = Instant::now();
            let fallback_context =
                match lingxi_core::host::refusal_driver::current_fallback_target() {
                    Some(context) => context,
                    None => {
                        crate::query_model::fallback_target_context(orch, &params.model, None).await
                    }
                };
            crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                model: params.model.clone(),
                profile: params.profile.clone(),
            });
            let outcome = lingxi_core::host::refusal_driver::scope_fallback_target(
                fallback_context,
                call_api_with_ptl_recovery(
                    orch,
                    params.system.as_ref(),
                    params.skip_global_cache_for_system_prompt,
                    &params.model,
                    params.profile.as_deref(),
                    params.history,
                    params.rewriter,
                    params.tools,
                    params.max_tokens,
                    params.deferred,
                    params.date_change,
                    params.reminders.clone(),
                    &mut params.guarded_async_hook_reminders,
                    &params.context_announcements,
                    params.cost_scope.as_ref(),
                ),
            )
            .await?;
            let recovered_physical = match &outcome {
                PtlCallOutcome::Response {
                    response,
                    request_history,
                } => vec![
                    PhysicalStepResponse::capture(
                        orch,
                        response.as_ref().clone(),
                        request_history.clone(),
                        model.clone(),
                        profile.clone(),
                        started.elapsed(),
                        &observation,
                    )
                    .await,
                ],
                _ => Vec::new(),
            };
            return Ok(Answer {
                outcome,
                preceding_responses: Vec::new(),
                model,
                profile,
                requested_model: true,
                physical_responses: recovered_physical,
                response_request_history_source: Some(RequestHistorySource::PhysicalRequest),
            });
        }
        return Err(OrchestratorError::Internal(format!(
            "turn.step Mod stream failed: {error}"
        )));
    }
    let tail = {
        let wire = wire.lock().unwrap();
        decoder.lock().unwrap().finish(&wire)
    };
    emitted.lock().unwrap().extend(tail);
    let events = std::mem::take(&mut *emitted.lock().unwrap());
    let mut responses = Vec::new();
    let mut pending = Vec::new();
    for event in events {
        let terminal = matches!(
            event,
            HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
        );
        pending.push(event);
        if terminal {
            let response = llm_runtime::stream_accumulator::accumulate_stream_salvaging(
                futures::stream::iter(std::mem::take(&mut pending).into_iter().map(Ok)).boxed(),
            )
            .await
            .map_err(|(_, error)| OrchestratorError::Streaming(error))?;
            responses.push(response);
        }
    }
    if !pending.is_empty() {
        return Err(OrchestratorError::Streaming(
            llm_runtime::LlmError::StreamInterrupted {
                message: "turn.step ended with an incomplete assistant response".into(),
            },
        ));
    }
    let response = responses.pop().ok_or_else(|| {
        OrchestratorError::Streaming(llm_runtime::LlmError::StreamInterrupted {
            message: "turn.step produced no assistant response".into(),
        })
    })?;
    let physical_responses = std::mem::take(&mut *physical_responses.lock().unwrap());
    let matching_physical_history = physical_responses
        .iter()
        .find(|physical| physical.response.id == response.id)
        .map(|physical| physical.request_history.clone());
    let (request_history, response_request_history_source) = match matching_physical_history {
        Some(request_history) => (request_history, Some(RequestHistorySource::PhysicalRequest)),
        None => (
            params.history.clone(),
            Some(RequestHistorySource::TurnStepInputForSyntheticResponse),
        ),
    };
    Ok(Answer {
        outcome: PtlCallOutcome::Response {
            response: Box::new(response),
            request_history,
        },
        preceding_responses: responses,
        model,
        profile,
        requested_model: !physical_responses.is_empty(),
        physical_responses,
        response_request_history_source,
    })
}
