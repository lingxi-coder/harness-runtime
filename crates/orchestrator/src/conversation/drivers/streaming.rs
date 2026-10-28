use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use futures::{future::BoxFuture, StreamExt as _};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use llm_runtime::model::retry_scope::ModelCallRetryScope;
use llm_runtime::{HistoryEvent, LlmError};
use tokio_util::sync::CancellationToken;

use super::super::hooks_impl::ModPromptScreen;
use super::{
    is_env_truthy, llm_response_to_pumped_turn, loop_state, prepare, pumped_assistant_message,
    token_aborted, QueuedPromptInput, StepExit, StreamingIterationDisposition, TurnLoopState,
};
use crate::conversation::{
    assistant_usage_value, classify_api_error, output_accounting_impl, ApiErrorEnvelope,
    ConversationOrchestrator, ConversationOutcome, ModelCallPath, OutgoingHistoryRewriter,
    PreparedContextAnnouncements, QueryUserInterruptionKind, INTERRUPT_MESSAGE,
};
use crate::error::OrchestratorError;
use crate::mod_turn_step::{TurnStepDecoder, TurnStepEncoder};
use crate::streaming_loop::{
    joined_visible_text_projection, visible_text_projection, ExecutorPump,
};
use crate::turn_loop::{
    call_api_with_ptl_recovery, surface_prompt_too_long, surface_rapid_refill_thrashing,
    PtlCallOutcome, MAX_OUTPUT_TOKENS_RECOVERY_LIMIT,
};
use hooks::attachment::HookPublicationGuard;

#[cfg(test)]
#[path = "streaming_recovery_tests.rs"]
mod recovery_tests;

pub(super) fn ensure_mod_recovery_accepted() -> Result<(), OrchestratorError> {
    hooks::mods::check_mod_turn_recovery_failure().map_err(|error| match error {
        hooks::RecoveryAdmissionError::Interrupted => OrchestratorError::RecoveryInterrupted,
        hooks::RecoveryAdmissionError::Failure(error) => OrchestratorError::Internal(error),
    })
}

// Cancel request opening before the first response stream exists. This is safe
// for the foreground turn: no stream event has dispatched tools or produced a
// partial assistant row yet. Once the stream is open, the pump owns the more
// granular cancellation path that preserves completed blocks and settles tools.
async fn open_cancelable_stream<F>(
    cancel: &Option<CancellationToken>,
    request: F,
) -> Result<futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>
where
    F: std::future::Future<
        Output = Result<
            futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
            LlmError,
        >,
    >,
{
    if let Some(cancel) = cancel {
        return tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(futures::stream::empty().boxed()),
            result = request => result,
        };
    }
    request.await
}

async fn side_model_request<F: std::future::Future>(
    orch: &ConversationOrchestrator,
    cancel: &Option<CancellationToken>,
    request: F,
) -> Option<F::Output> {
    if orch.side_chat {
        if let Some(cancel) = cancel {
            return tokio::select! { biased; () = cancel.cancelled() => None, result = request => Some(result) };
        }
    }
    Some(request.await)
}

fn observe_main_dispatch(
    observer: Option<llm_runtime::RequestDispatchObserver>,
    output: &Arc<dyn lingxi_core::host::OutputStream>,
) -> Option<llm_runtime::RequestDispatchObserver> {
    if !output.wants_response_timing() {
        return observer;
    }
    let output = output.clone();
    Some(llm_runtime::RequestDispatchObserver::new(move || {
        let at = std::time::Instant::now();
        if let Some(observer) = &observer {
            observer.mark_dispatched();
        }
        output.note_response_timing(
            lingxi_core::host::orchestrator::ResponseTimingEvent::RequestStarted,
            at,
        );
    }))
}

pub(super) struct StreamingTurnDriver<'a> {
    pub(super) orch: &'a ConversationOrchestrator,
    pub(super) prompt: &'a str,
    pub(super) images: Vec<lingxi_core::types::ImageSource>,
    pub(super) user_cancel: Option<CancellationToken>,
    pub(super) message_id: Option<MessageId>,
    pub(super) transient_rewake: bool,
    pub(super) in_human_turn: bool,
    pub(super) queued_inputs: Option<Vec<QueuedPromptInput>>,
    pub(super) row_token: Option<String>,
    pub(super) projected_content: Option<super::projected_content::ProjectedUserContent>,
}

struct PreparedStreamingIteration {
    step: prepare::PreparedTurnStep,
    assistant_id: MessageId,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    turn_id: String,
    display_hook_active: bool,
    cost_scope: Option<cost::CostSessionScope>,
}

enum PrepareStreamingOutcome {
    Ready(PreparedStreamingIteration),
    Complete(MessageId),
}

enum OpenedModelStream {
    Stream(futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>),
    Recovered(crate::streaming_loop::PumpedTurn),
}

/// Only a host-classified external delivery enters `session.receive` before
/// prompt admission. Composer and plugin prompts have their own events; an
/// unclassified local continuation must not be relabeled as bridge input.
pub(super) fn external_receive_kind(origin: &serde_json::Value) -> Option<&str> {
    match origin.get("kind").and_then(serde_json::Value::as_str)? {
        kind @ ("bridge" | "task-notification" | "scheduled-trigger" | "peer"
        | "peer-send-message" | "projects-relay" | "slack-ping") => Some(kind),
        _ => None,
    }
}

async fn append_recovered_assistant_row(
    orch: &ConversationOrchestrator,
    pumped: &mut crate::streaming_loop::PumpedTurn,
    assistant_id: MessageId,
) {
    let raw = pumped_assistant_message(pumped, assistant_id);
    // Session switching, clear, and resume share the turn gate with this
    // recovered output. Do not bind recovery acceptance to a fresh executor
    // generation fence that did not produce the row.
    let accepted = orch.append_streamed_query_row(&raw, None).await;
    if let ConversationMessage::Assistant { content, .. } = accepted {
        // Native's query row is the append-through result. q/D reconstructs
        // source ToolUse identity/input; `query_history` remains the separate
        // pre-query snapshot used by W1.
        pumped.assistant_blocks = content;
    }
}

struct OpenedStreamingIteration<'a> {
    opened: OpenedModelStream,
    exec: crate::streaming_executor::StreamingToolExecutor<'a>,
    /// Exact query request messages frozen before the stream opened. Streaming
    /// tool dispatch uses this even when its executor starts queued work later.
    query_history: Vec<ConversationMessage>,
    retry_scope: ModelCallRetryScope,
    model: String,
    model_profile: Option<String>,
    outgoing_history_rewriter: Option<Arc<dyn OutgoingHistoryRewriter>>,
    turn_reminders: Vec<ConversationMessage>,
    tool_result_reminder_suffixes: Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix>,
    native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
    guarded_async_hook_reminders: Vec<(MessageId, Arc<dyn HookPublicationGuard>)>,
    context_announcements: PreparedContextAnnouncements,
    wire_tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    available_tool_names: Option<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
    skip_global_cache_for_system_prompt: bool,
    deferred_reminder: Option<ConversationMessage>,
    date_change_reminder: Option<ConversationMessage>,
    assistant_id: MessageId,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    turn_id: String,
    display_hook_active: bool,
    turn_step_hook_active: bool,
    mod_model_state: Option<Arc<StdMutex<Option<(String, Option<String>)>>>>,
    api_call_started: std::time::Instant,
    stream_started: tokio::time::Instant,
    api_success_message_count: u32,
    api_success_message_tokens: u64,
    did_fall_back_to_non_streaming: bool,
    cost_scope: Option<cost::CostSessionScope>,
    cost_receipt: Option<cost::CostResponseReceipt>,
}

enum OpenStreamingOutcome<'a> {
    Opened(OpenedStreamingIteration<'a>),
    Complete(MessageId),
    ModelFallback,
}

struct PumpedStreamingIteration<'a> {
    pumped: crate::streaming_loop::PumpedTurn,
    settlement: crate::streaming_loop::StreamToolSettlement,
    next_mod_response: Option<OpenedStreamingIteration<'a>>,
    exec: crate::streaming_executor::StreamingToolExecutor<'a>,
    request_model: String,
    model: String,
    model_profile: Option<String>,
    wire_tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    assistant_id: MessageId,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    turn_id: String,
    display_hook_active: bool,
    api_call_started: std::time::Instant,
    api_success_message_count: u32,
    api_success_message_tokens: u64,
    did_fall_back_to_non_streaming: bool,
    /// Exact physical history for the successful stream or its recovery request.
    cache_request_history: Vec<ConversationMessage>,
    /// Inline Native context is removed from cache-safe fork history by identity.
    cache_inline_context: Option<ConversationMessage>,
    /// PTL/529 rows passed through session.append before W1 dispatch, so the
    /// terminal per-block writer must not dispatch the hook again.
    assistant_session_append_dispatched: bool,
    pre_batch_mcp_tool_count: usize,
    cost_receipt: Option<cost::CostResponseReceipt>,
}

enum PumpStreamingOutcome<'a> {
    Pumped(PumpedStreamingIteration<'a>),
    Complete(MessageId),
    ModelFallback,
    /// A user interrupt aborted the response before it produced output.
    Interrupted,
}

enum StreamErrorFallback<'a> {
    Recovered {
        pumped: crate::streaming_loop::PumpedTurn,
        executor: crate::streaming_executor::StreamingToolExecutor<'a>,
        request_history: Vec<ConversationMessage>,
        cost_receipt: Option<cost::CostResponseReceipt>,
    },
    Declined(MessageId),
    Cancelled,
}

type ModStepFrame = (
    Result<HistoryEvent, LlmError>,
    tokio::sync::oneshot::Sender<()>,
);

enum PendingModStepOutput {
    Event(HistoryEvent),
    Thinking(String),
}

async fn send_mod_step_output(
    sender: &tokio::sync::mpsc::Sender<ModStepFrame>,
    output: &Arc<dyn lingxi_core::host::OutputStream>,
    pending: Vec<PendingModStepOutput>,
) -> Result<(), hooks::mods::ModError> {
    for item in pending {
        match item {
            PendingModStepOutput::Thinking(text) => output.emit_thinking(&text, None).await,
            PendingModStepOutput::Event(event) => {
                let terminal = matches!(
                    event,
                    HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
                );
                let (ack, received) = tokio::sync::oneshot::channel();
                sender.send((Ok(event), ack)).await.map_err(|_| {
                    hooks::mods::ModError::Unavailable("turn.step consumer closed".into())
                })?;
                if !terminal {
                    received.await.map_err(|_| {
                        hooks::mods::ModError::Unavailable("turn.step consumer closed".into())
                    })?;
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn mod_turn_step_stream(
    host: Arc<hooks::mods::ModHost>,
    session: Option<Arc<dyn hooks::mods::ModSessionContext>>,
    recovery_leases: Option<crate::conversation::ToolDispatchRecoveryLeases>,
    api: Arc<dyn crate::conversation::StreamingApiClient>,
    output: Arc<dyn lingxi_core::host::OutputStream>,
    main_api: Arc<dyn crate::conversation::OrchestratorApiClient>,
    model_policy: Option<Arc<dyn hooks::mods::ModSettingsReader>>,
    catalog_restriction: llm_runtime::model::allowlist::CatalogRestriction,
    model_overrides: std::collections::BTreeMap<String, String>,
    default_model: String,
    input: serde_json::Value,
    model_profile: Option<String>,
    configured_model_source: Option<Utf16JsonProjection>,
    system_prompt: Option<
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
    >,
    snapshot: Vec<ConversationMessage>,
    guarded_async_hook_reminders: Vec<(
        MessageId,
        Arc<dyn hooks::attachment::HookPublicationGuard>,
    )>,
    tools: Vec<llm_runtime::ToolDeclaration>,
    tool_result_reminder_suffixes: Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix>,
    skip_global_cache_for_system_prompt: bool,
    query_source: String,
    output_observation: Option<output_accounting_impl::MainOutputObservation>,
    retry_scope: ModelCallRetryScope,
    request_capture: llm_runtime::prompt_cache::RequestCapture,
    request_dispatch_observer: Option<llm_runtime::RequestDispatchObserver>,
    native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
    typed_attachment_ids: std::collections::HashSet<MessageId>,
    task_deliveries: std::collections::HashMap<
        lingxi_core::types::MessageId,
        lingxi_core::host::task_delivery::TaskDeliveryRequest,
    >,
) -> (
    futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
    Arc<StdMutex<Option<(String, Option<String>)>>>,
) {
    let wire = Arc::new(StdMutex::new(TurnStepEncoder::default()));
    let input_projection = Utf16JsonProjection::from_serde_value(&input)
        .expect("turn.step dispatch input is generated from valid JSON");
    let model = input["model"].as_str().unwrap_or_default().to_owned();
    let original_effort = input["effort"].as_str().map(str::to_owned);
    let decoder = Arc::new(StdMutex::new(TurnStepDecoder::new(model.clone())));
    let effective_model = Arc::new(StdMutex::new(None));
    let fallback_tools = tools.clone();
    let physical_error = Arc::new(StdMutex::new(None::<LlmError>));
    let observation = Arc::new(StdMutex::new(output_observation));
    let publication_guards = Arc::new(StdMutex::new(guarded_async_hook_reminders));
    let (sender, receiver) = tokio::sync::mpsc::channel::<ModStepFrame>(1);
    let worker_sender = sender.clone();
    let worker_wire = wire.clone();
    let worker_decoder = decoder.clone();
    let worker_model = effective_model.clone();
    let worker_error = physical_error.clone();
    tokio::spawn(lingxi_core::host::model_safety::bind_current_model_safety(
        telemetry::otel::bind_current_trace_context(async move {
            // This worker is independent of the consumer and Tokio does not
            // inherit task-local recovery scopes. Retain the originating turn
            // until the actual Mod dispatch and its stream work have finished.
            let detached_mod_leases = recovery_leases
                .as_ref()
                .map(|leases| (Arc::clone(&leases.execution), Arc::clone(&leases.turn_work)));
            let _recovery_leases = recovery_leases;
            let finish_wire = worker_wire.clone();
            let requested_model = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let source_requested = requested_model.clone();
            let pending_output = Arc::new(StdMutex::new(Vec::<PendingModStepOutput>::new()));
            let fallback_api = api.clone();
            let fallback_model_source = configured_model_source.clone();
            let fallback_model = model.clone();
            let fallback_profile = model_profile.clone();
            let fallback_system = system_prompt.clone();
            let fallback_snapshot = snapshot.clone();
            let fallback_tool_result_reminder_suffixes = tool_result_reminder_suffixes.clone();
            let fallback_native_anthropic_request_context =
                native_anthropic_request_context.clone();
            let fallback_typed_attachment_ids = typed_attachment_ids.clone();
            let fallback_task_deliveries = task_deliveries.clone();
            let fallback_publication_guards = publication_guards.clone();
            let fallback_query_source = query_source.clone();
            let fallback_observation = observation.clone();
            let fallback_output = output.clone();
            let fallback_retry_scope = retry_scope.clone();
            let fallback_request_capture = request_capture.clone();
            let source_retry_scope = retry_scope.clone();
            let source_request_capture = request_capture.clone();
            let source_tool_result_reminder_suffixes = tool_result_reminder_suffixes.clone();
            let source_native_anthropic_request_context = native_anthropic_request_context.clone();
            let source_typed_attachment_ids = typed_attachment_ids;
            let source_task_deliveries = task_deliveries;
            let source_dispatch_observer = request_dispatch_observer.clone();
            let source_output = output.clone();
            let source_model_source = configured_model_source.clone();
            let source = move |forwarded: Utf16JsonProjection| {
                let api = api.clone();
                let main_api = main_api.clone();
                let model_policy = model_policy.clone();
                let catalog_restriction = catalog_restriction.clone();
                let model_overrides = model_overrides.clone();
                let default_model = default_model.clone();
                let original_model = model.clone();
                let original_effort = original_effort.clone();
                let model_profile = model_profile.clone();
                let system_prompt = system_prompt.clone();
                let snapshot = snapshot.clone();
                let publication_guards = publication_guards.clone();
                let tools = tools.clone();
                let query_source = query_source.clone();
                let observation = observation.clone();
                let wire = worker_wire.clone();
                let effective_model = worker_model.clone();
                let physical_error = worker_error.clone();
                let requested_model = source_requested.clone();
                let retry_scope = source_retry_scope.clone();
                let request_capture = source_request_capture.clone();
                let request_dispatch_observer =
                    observe_main_dispatch(source_dispatch_observer.clone(), &source_output);
                let tool_result_reminder_suffixes = source_tool_result_reminder_suffixes.clone();
                let native_anthropic_request_context =
                    source_native_anthropic_request_context.clone();
                let typed_attachment_ids = source_typed_attachment_ids.clone();
                let task_deliveries = source_task_deliveries.clone();
                let configured_model_source = source_model_source.clone();
                async move {
                    let requested = forwarded.string_at("/model").unwrap_or_default().to_owned();
                    let default_sentinel =
                        lingxi_core::host::effort::trim_js_whitespace(&requested)
                            .eq_ignore_ascii_case("default");
                    // Keep the route identity alive independently from the
                    // output model/profile selected below. The later source
                    // carrier comparison must still observe the exact request
                    // route after fallback handling consumes owned values.
                    let route_model = if default_sentinel {
                        default_model.clone()
                    } else {
                        requested.clone()
                    };
                    let route_profile = if default_sentinel {
                        None
                    } else {
                        model_profile.clone()
                    };
                    let resolved = main_api
                        .resolve_media_route(&route_model, route_profile.as_deref())
                        .or_else(|_| main_api.resolve_media_route(&route_model, None))
                        .ok();
                    let resolved_model = resolved.as_ref().map_or(route_model.as_str(), |route| {
                        route.main.request_model.as_str()
                    });
                    let is_explicit_model_override = requested.as_str() != original_model.as_str();
                    let native_catalog_route = resolved.as_ref().is_some_and(
                        crate::ConversationOrchestrator::route_uses_native_model_catalog,
                    );
                    let same_active_model = is_explicit_model_override
                        && crate::ConversationOrchestrator::same_native_model_identity(
                            main_api.as_ref(),
                            &route_model,
                            route_profile.as_deref(),
                            &original_model,
                            model_profile.as_deref(),
                        );
                    let same_default_model = is_explicit_model_override
                        && crate::ConversationOrchestrator::matches_native_default_model_identity(
                            main_api.as_ref(),
                            &route_model,
                            route_profile.as_deref(),
                            &default_model,
                            None,
                        );
                    let denied = if is_explicit_model_override
                        && native_catalog_route
                        && !same_active_model
                        && !same_default_model
                    {
                        let from_reader = if let Some(policy) = model_policy.as_ref() {
                            policy.model_allowed(resolved_model).await?
                        } else {
                            None
                        };
                        !from_reader.unwrap_or_else(|| {
                            llm_runtime::model::allowlist::model_allowed_under_catalog(
                                resolved_model,
                                &catalog_restriction,
                                Some(&model_overrides),
                            )
                        })
                    } else {
                        false
                    };
                    let model = if denied {
                        tracing::warn!(requested, "turn.step model rewrite denied by policy");
                        original_model.clone()
                    } else if default_sentinel {
                        resolved_model.to_owned()
                    } else {
                        requested
                    };
                    let profile = if denied {
                        model_profile.clone()
                    } else {
                        resolved
                            .map(|route| route.main.profile_name)
                            .or(if default_sentinel {
                                None
                            } else {
                                model_profile.clone()
                            })
                    };
                    let effort_override = forwarded
                        .string_at("/effort")
                        .filter(|effort| Some(*effort) != original_effort.as_deref());
                    *effective_model.lock().unwrap() = Some((model.clone(), profile.clone()));
                    requested_model.store(true, std::sync::atomic::Ordering::Release);
                    let mut snapshot = snapshot;
                    let mut turn_reminders = Vec::new();
                    let mut guarded_reminders = publication_guards
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                        &mut snapshot,
                        &mut turn_reminders,
                        &mut guarded_reminders,
                    );
                    let request_dispatch_admission =
                        crate::prompt::async_hook_response::request_dispatch_admission(
                            &snapshot,
                            &guarded_reminders,
                        );
                    let model_source =
                        crate::ConversationOrchestrator::configured_source_for_effective_route(
                            main_api.as_ref(),
                            &model,
                            profile.as_deref(),
                            &original_model,
                            model_profile.as_deref(),
                            configured_model_source.clone(),
                        );
                    let stream = retry_scope
                        .run(request_capture.scope(api.stream_with_effort_override(
                            &model,
                            profile.as_deref(),
                            system_prompt.as_ref(),
                            snapshot,
                            tools,
                            None,
                            effort_override,
                            tool_result_reminder_suffixes,
                            &query_source,
                            skip_global_cache_for_system_prompt,
                            request_dispatch_admission,
                            request_dispatch_observer.clone(),
                            native_anthropic_request_context,
                            typed_attachment_ids,
                            task_deliveries,
                            model_source,
                        )))
                        .await
                        .map_err(|error| {
                            *physical_error.lock().unwrap() = Some(error.clone());
                            hooks::mods::ModError::Hook(error.to_string())
                        })?;
                    let observation = observation
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(output_accounting_impl::MainOutputObservation::fork);
                    let stream = output_accounting_impl::account_stream(stream, observation);
                    let chunk_wire = wire.clone();
                    let result_wire = wire.clone();
                    let source_refs = Arc::new(StdMutex::new(Vec::new()));
                    let chunk_refs = source_refs.clone();
                    let response_events = Arc::new(StdMutex::new(Vec::new()));
                    let stream = stream.then(move |item| {
                    let response_events = response_events.clone();
                    let chunk_wire = chunk_wire.clone();
                    let chunk_refs = chunk_refs.clone();
                    let physical_error = physical_error.clone();
                    async move {
                        match item {
                            Ok(event) => {
                                let stopped = matches!(event, HistoryEvent::MessageStop);
                                if matches!(event, HistoryEvent::Completed { .. }) {
                                    response_events.lock().unwrap().clear();
                                } else {
                                    response_events.lock().unwrap().push(event.clone());
                                }
                                let streamed_response = if stopped {
                                    let events =
                                        std::mem::take(&mut *response_events.lock().unwrap());
                                    llm_runtime::stream_accumulator::accumulate_stream_salvaging(
                                        futures::stream::iter(events.into_iter().map(Ok)).boxed(),
                                    )
                                    .await
                                    .ok()
                                } else {
                                    None
                                };
                                let held = {
                                    let mut wire = chunk_wire.lock().unwrap();
                                    let held = wire.push(event).clone();
                                    if let Some(response) = streamed_response {
                                        wire.note_stream_response(held.reference, response);
                                    }
                                    held
                                };
                                chunk_refs.lock().unwrap().push(held.reference);
                                Ok(held.chunk)
                            }
                            Err(error) => {
                                *physical_error.lock().unwrap() = Some(error.clone());
                                Err(hooks::mods::ModError::Hook(error.to_string()))
                            }
                        }
                    }
                });
                    let turn_id = forwarded
                        .string_at("/turnId")
                        .unwrap_or_default()
                        .to_owned();
                    let index = forwarded
                        .number_at("/index")
                        .filter(|index| {
                            index.is_finite()
                                && *index >= 0.0
                                && index.fract() == 0.0
                                && *index <= u64::MAX as f64
                        })
                        .map(|index| index as u64 as u32)
                        .unwrap_or_default();
                    Ok(hooks::mods::ModStreamSource::new(stream, move || {
                        let references = source_refs.lock().unwrap().clone();
                        let result = result_wire.lock().unwrap().result_for_references(
                            &turn_id,
                            index,
                            &references,
                        );
                        Ok(result)
                    }))
                }
            };
            let output_requested = requested_model.clone();
            let output_pending = pending_output.clone();
            let on_chunk = move |chunk: Utf16JsonProjection| {
                let wire = wire.clone();
                let decoder = worker_decoder.clone();
                let sender = worker_sender.clone();
                let output = output.clone();
                let requested_model = output_requested.clone();
                let pending_output = output_pending.clone();
                async move {
                    let mut actions = Vec::new();
                    if chunk.string_at("/kind") == Some("thinking") {
                        if let Some(text) = chunk.string_at("/text") {
                            actions.push(PendingModStepOutput::Thinking(text.to_owned()));
                        }
                    }
                    let events = {
                        let wire = wire.lock().unwrap();
                        decoder.lock().unwrap().consume(&chunk, &wire)
                    };
                    actions.extend(events.into_iter().map(PendingModStepOutput::Event));
                    if !requested_model.load(std::sync::atomic::Ordering::Acquire) {
                        let mut immediate = Vec::new();
                        for action in actions {
                            if matches!(
                                action,
                                PendingModStepOutput::Event(HistoryEvent::Completed { .. })
                            ) {
                                pending_output.lock().unwrap().push(action);
                            } else {
                                immediate.push(action);
                            }
                        }
                        return send_mod_step_output(&sender, &output, immediate).await;
                    }
                    let mut ready = std::mem::take(&mut *pending_output.lock().unwrap());
                    ready.extend(actions);
                    send_mod_step_output(&sender, &output, ready).await
                }
            };
            let dispatch = host.dispatch_turn_step_stream_with_session(
                input_projection,
                session,
                source,
                on_chunk,
            );
            let dispatch = async move {
                if let Some(leases) = detached_mod_leases {
                    hooks::mods::with_detached_mod_recovery_leases(leases, dispatch).await
                } else {
                    dispatch.await
                }
            };
            let outcome = tokio::select! {
                biased;
                () = sender.closed() => return,
                result = dispatch => result,
            };
            match outcome {
                Ok(_) => {
                    let tail = {
                        let wire = finish_wire.lock().unwrap();
                        decoder.lock().unwrap().finish(&wire)
                    };
                    let mut ready = std::mem::take(&mut *pending_output.lock().unwrap());
                    ready.extend(tail.into_iter().map(PendingModStepOutput::Event));
                    let _ = send_mod_step_output(&sender, &fallback_output, ready).await;
                }
                Err(error) => {
                    if !requested_model.load(std::sync::atomic::Ordering::Acquire) {
                        // Native `Jn` holds only complete assistant records before
                        // a request; stream events already reached the consumer.
                        // A failed chain without a request drops those held records.
                        pending_output.lock().unwrap().clear();
                        let mut fallback_snapshot = fallback_snapshot;
                        let mut fallback_reminders = Vec::new();
                        let mut fallback_guards = fallback_publication_guards
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clone();
                        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                            &mut fallback_snapshot,
                            &mut fallback_reminders,
                            &mut fallback_guards,
                        );
                        let request_dispatch_admission =
                            crate::prompt::async_hook_response::request_dispatch_admission(
                                &fallback_snapshot,
                                &fallback_guards,
                            );
                        let fallback_request =
                            fallback_retry_scope.run(fallback_request_capture.scope(
                                fallback_api.stream_with_effort_override(
                                    &fallback_model,
                                    fallback_profile.as_deref(),
                                    fallback_system.as_ref(),
                                    fallback_snapshot,
                                    fallback_tools,
                                    None,
                                    None,
                                    fallback_tool_result_reminder_suffixes,
                                    &fallback_query_source,
                                    skip_global_cache_for_system_prompt,
                                    request_dispatch_admission,
                                    observe_main_dispatch(
                                        request_dispatch_observer.clone(),
                                        &fallback_output,
                                    ),
                                    fallback_native_anthropic_request_context.clone(),
                                    fallback_typed_attachment_ids,
                                    fallback_task_deliveries,
                                    fallback_model_source,
                                ),
                            ));
                        let stream = tokio::select! {
                            biased;
                            () = sender.closed() => return,
                            result = fallback_request => result,
                        };
                        match stream {
                            Ok(stream) => {
                                let observation = fallback_observation
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .map(output_accounting_impl::MainOutputObservation::fork);
                                let mut stream =
                                    output_accounting_impl::account_stream(stream, observation);
                                loop {
                                    let item = tokio::select! {
                                        biased;
                                        () = sender.closed() => return,
                                        item = stream.next() => item,
                                    };
                                    let Some(item) = item else { break };
                                    let terminal = matches!(
                                        item,
                                        Ok(HistoryEvent::MessageStop
                                            | HistoryEvent::Completed { .. })
                                    );
                                    let (ack, received) = tokio::sync::oneshot::channel();
                                    if sender.send((item, ack)).await.is_err() {
                                        return;
                                    }
                                    if !terminal && received.await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                let (ack, _) = tokio::sync::oneshot::channel();
                                let _ = sender.send((Err(error), ack)).await;
                            }
                        }
                        return;
                    }
                    let pending = std::mem::take(&mut *pending_output.lock().unwrap());
                    if send_mod_step_output(&sender, &fallback_output, pending)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let typed = physical_error.lock().unwrap().take().unwrap_or_else(|| {
                        LlmError::StreamInterrupted {
                            message: error.to_string(),
                        }
                    });
                    let (ack, _) = tokio::sync::oneshot::channel();
                    let _ = sender.send((Err(typed), ack)).await;
                }
            }
        }),
    ));
    let stream = futures::stream::unfold(
        (receiver, None::<tokio::sync::oneshot::Sender<()>>),
        |(mut receiver, previous_ack)| async move {
            if let Some(ack) = previous_ack {
                let _ = ack.send(());
            }
            let (item, ack) = receiver.recv().await?;
            Some((item, (receiver, Some(ack))))
        },
    )
    .fuse()
    .boxed();
    (stream, effective_model)
}

enum FinalizedStreamingIteration<'a> {
    Complete(MessageId),
    Finalized(FinalizedStreamingData<'a>),
}

struct FinalizedStreamingData<'a> {
    pumped: crate::streaming_loop::PumpedTurn,
    next_mod_response: Option<OpenedStreamingIteration<'a>>,
    request_model: String,
    native_catalog_refusal: bool,
    assistant_id: MessageId,
    tool_prevent_continuation: bool,
    post_tool_batch_dispatch: crate::turn_loop::PostToolBatchDispatch,
    pre_batch_mcp_tool_count: usize,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    aborted_during_stream: bool,
    refusal_tool_uses_settled: bool,
}

impl StreamingTurnDriver<'_> {
    fn begin_stream_cost_response(
        orch: &ConversationOrchestrator,
        cost_scope: Option<&cost::CostSessionScope>,
        model: &str,
        model_profile: Option<&str>,
        pumped: &crate::streaming_loop::PumpedTurn,
        duration: std::time::Duration,
    ) -> Option<cost::CostResponseReceipt> {
        let usage = pumped.usage.as_ref();
        if usage.is_none() && !pumped.native_server_fallback_quote {
            return None;
        }
        orch.model_runtime.cost_tracker.as_ref()?;
        let scope =
            cost_scope.expect("a wired cost tracker captured its scope before stream dispatch");
        let cost_usage = usage
            .map(crate::cost_wiring::llm_usage_to_cost_usage)
            .unwrap_or_default();
        let native_quote = if pumped.cost_quote_observed {
            pumped.native_server_fallback_quote
        } else {
            usage.is_some_and(|usage| {
                usage
                    .provider_metadata
                    .payload_at("/stream")
                    .is_some_and(|metadata| {
                        crate::cost_wiring::has_native_fallback_quote(&metadata)
                    })
            })
        };
        let (quoted_model, pricing) =
            crate::cost_wiring::response_pricing(pumped.cost_quote.as_ref(), native_quote);
        let usage_fallback_model = usage.and_then(|usage| {
            usage
                .provider_metadata
                .payload_at("/stream")
                .and_then(|metadata| crate::cost_wiring::native_fallback_cost_model(&metadata))
        });
        let model_ref = quoted_model.unwrap_or_else(|| {
            let billing_model = if native_quote {
                pumped
                    .native_cost_model
                    .as_deref()
                    .or(usage_fallback_model.as_deref())
                    .or(pumped.served_model.as_deref())
                    .unwrap_or(model)
            } else {
                model
            };
            crate::cost_wiring::model_ref_from_string(billing_model, model_profile)
        });
        Some(scope.submit_model_response_with_pricing(
            cost::CostModelResponse {
                model_ref,
                usage: cost_usage,
                duration,
                retries: orch.streaming_api.last_retry_count(),
                cache_read_input_tokens: usage.map_or(0, |usage| usage.counts().cache_read_tokens),
                cache_creation_input_tokens:
                    usage.map_or(0, |usage| usage.counts().cache_write_tokens),
                is_batch_request: false,
                bus: orch.model_runtime.analytics_bus.clone(),
            },
            pricing,
            if usage.is_some_and(|usage| usage.report.usage.is_some()) {
                cost::CostResponseMeasurement::Observed
            } else {
                cost::CostResponseMeasurement::Missing
            },
        ))
    }

    async fn prepare_iteration(
        orch: &ConversationOrchestrator,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
        in_human_turn: bool,
        previous_step: Option<&prepare::PreparedTurnStep>,
    ) -> Result<PrepareStreamingOutcome, OrchestratorError> {
        // Shared per-step preparation. The token goes IN rather than racing
        // this call from outside: streaming must not be dropped mid-flight by a
        // `select!`, because that discards in-flight tool results (see the
        // DEFERRED-3 note on `run_turn_streaming_inputs_locked`).
        let is_regular_user_prompt = orch.regular_user_prompt_for_model_step().await;
        let mut prepared = if let Some(previous) = previous_step {
            orch.reprepare_model_fallback(system_prompt.as_ref(), user_cancel.as_ref(), previous)
                .await?
        } else {
            orch.prepare_turn_step(
                ModelCallPath::Streaming,
                system_prompt.as_ref(),
                in_human_turn,
                is_regular_user_prompt,
                user_cancel.as_ref(),
            )
            .await?
        };
        ensure_mod_recovery_accepted()?;
        // A cancelled preparation has not been admitted. Keep request-context
        // and shared context attachments staged for the next accepted turn.
        if !token_aborted(user_cancel) {
            orch.persist_prepared_turn_attachments(&mut prepared).await;
        }

        // Capture the originating cost scope so a later session switch cannot
        // redirect observed usage to the active session. This used to be read
        // between the peer-inbox drain and the snapshot; the slot is only ever
        // written by `execute_clear_session` / `execute_resume_session`, never
        // during a turn, so reading it here is the same value.
        let mut cost_scope = orch
            .model_runtime
            .cost_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if cost_scope.is_none() {
            if let Some(tracker) = orch.model_runtime.cost_tracker.as_ref() {
                let session_id = orch.session.lock().await.session_id;
                cost_scope = Some(tracker.session_scope(session_id));
            }
        }

        // RECOV.1: blocking-limit preempt — the streaming twin of the batched
        // `call_api_with_ptl_recovery` step (1) (TS `query.ts:592-648`). If the
        // pre-call prompt is already at the hard blocking limit
        // (`token_usage >= effective_window − MANUAL_COMPACT_BUFFER_TOKENS`),
        // surface the byte-exact `PROMPT_TOO_LONG_ERROR_MESSAGE` and END the
        // turn WITHOUT opening the stream — mirroring the batched path (which
        // returns `PtlCallOutcome::PromptTooLong` ⇒ ends with stop_reason
        // `"prompt_too_long"`). Use the exact same custom beta set as the
        // provider request so context-1m sessions are not preempted at the
        // default 200k boundary.
        let warning = orch.api.context_warning(
            compaction::grouping::estimate_tokens_for_range(&prepared.snapshot),
            &prepared.model,
            prepared.model_profile.as_deref(),
        );
        if warning.is_at_blocking_limit {
            tracing::warn!(
                model = %prepared.model,
                "prompt at blocking limit — preempting before stream"
            );
            let id = surface_prompt_too_long(orch).await;
            // PROACTIVE preempt ⇒ terminal reason `"blocking_limit"` (distinct
            // from the reactive-exhausted `prompt_too_long`), mirroring the
            // batched path's `PtlCallOutcome::BlockingLimit`. RECOV.2 chokepoint:
            // `handle_stop_at_end` treats `"blocking_limit"` as an api-error end
            // (fires `StopFailure`, skips the normal `Stop` hooks, returns
            // `FallThrough`), so the directive is discarded and the normal
            // end-of-turn tail runs — exactly mirroring the batched path.
            let _ = orch
                .handle_stop_at_end(
                    "blocking_limit",
                    &mut loop_state.stop_hook_active,
                    &mut loop_state.stop_hook_blocking_count,
                    loop_state.turn_count,
                    id,
                    token_aborted(user_cancel),
                )
                .await;
            let cost = orch.snapshot_cost_real().await;
            orch.mark_mod_turn_error();
            orch.emit_turn_terminal("blocking_limit", &cost).await;
            return Ok(PrepareStreamingOutcome::Complete(id));
        }
        // Past the preempt: this step's snapshot WILL be sent, so the
        // `date_change` reminder it carries counts as delivered.
        orch.commit_date_change_reminder();

        // Mid-stream tool dispatch (claude-code `query.ts:562` + `837-844`):
        // create the executor + pre-allocate this turn's assistant id BEFORE
        // opening the stream, so each `tool_use` block can be `add_tool`'d and
        // dispatched the moment it streams in (instead of collecting all
        // tool_uses and only starting them after the stream ends). This is
        // BYTE-EQUIVALENT: only WHEN tools start changes. The per-block
        // assistant JSONL persistence + per-result drain/persist still run
        // post-stream below (see `pump_stream_with_executor`'s contract).
        //
        // DEFERRED-3: hand the executor the turn's user-interrupt token (if
        // any) so it can reject in-flight/queued Cancel-behavior tools with the
        // REJECT_MESSAGE; `None` → identical to before.
        let assistant_id = MessageId::new();

        // P1-04 (cc 2.1.199 partial-stream finalize): set by the pump error
        // arm when a completed partial is finalized in place. Drives the
        // "API Error: … may be incomplete." notice (surfaced after the partial
        // is persisted) and the terminal turn-end below. Reset per turn.
        let partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause> = None;
        let partial_finalize_notice_id: Option<MessageId> = None;

        // hooks #39: MessageDisplay fires at the BEGIN of this assistant
        // message's stream (claude-code `begin(d)`, BIN off 208862320),
        // mirroring its `o={apiMessageId:d, messageId:randomUUID(),
        // turnId:r, index:0, …}` initialization. The `turn_id` is a fresh
        // per-turn UUID (`newTurn(){…; r=randomUUID()}`). Best-effort +
        // no-op when unregistered, so existing flows are byte-identical.
        let turn_id = loop_state.mod_turn_id.clone();
        orch.fire_message_display(&turn_id, assistant_id).await;

        // P2-04 (MessageDisplay `displayContent`): a registered
        // `MessageDisplay` hook makes the live pump SUPPRESS per-token text
        // deltas so the completed-message pass below renders the full
        // (possibly hook-substituted) text once — faithful to claude-code
        // `Qff` (BIN off 229876575), whose live render flows through the
        // display flush, not raw deltas. `false` (no hook) ⇒ byte-identical
        // live streaming. Cheap subscription gate (declared-event only).
        let display_hook_active = orch
            .hooks
            .has_hooks_for(&hooks::events::HookEventType::MessageDisplay)
            .await;

        Ok(PrepareStreamingOutcome::Ready(PreparedStreamingIteration {
            step: prepared,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            cost_scope,
        }))
    }

    fn open_iteration<'orch: 'call, 'call>(
        orch: &'orch ConversationOrchestrator,
        prepared: PreparedStreamingIteration,
        system_prompt: &'call Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &'call Option<CancellationToken>,
        loop_state: &'call mut TurnLoopState,
        retry_scope: &'call ModelCallRetryScope,
    ) -> BoxFuture<'call, Result<OpenStreamingOutcome<'orch>, OrchestratorError>> {
        Box::pin(StreamingTurnDriver::<'orch>::open_iteration_inner(
            orch,
            prepared,
            system_prompt,
            user_cancel,
            loop_state,
            retry_scope,
        ))
    }

    async fn open_iteration_inner<'a>(
        orch: &'a ConversationOrchestrator,
        prepared: PreparedStreamingIteration,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
        retry_scope: &ModelCallRetryScope,
    ) -> Result<OpenStreamingOutcome<'a>, OrchestratorError> {
        let PreparedStreamingIteration {
            step,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            cost_scope,
        } = prepared;

        let prepare::PreparedTurnStep {
            mut snapshot,
            mut model,
            mut model_profile,
            outgoing_history_rewriter,
            mut turn_reminders,
            tool_result_reminder_suffixes,
            mut native_anthropic_request_context,
            thinking_resumption_host_facts,
            mut guarded_async_hook_reminders,
            context_announcements,
            mut wire_tools,
            mut available_tool_names,
            mut skip_global_cache_for_system_prompt,
            deferred_reminder,
            date_change_reminder,
            ..
        } = step;
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut snapshot,
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
        );
        let mut system_override = None;
        if let Some(frozen) = loop_state.recovery.retry_generation.take() {
            let latest_user = |messages: &[ConversationMessage]| {
                messages
                    .iter()
                    .rev()
                    .find(|m| matches!(m, ConversationMessage::User { is_meta: false, .. }))
                    .map(ConversationMessage::id)
            };
            if latest_user(&snapshot) == latest_user(&frozen.messages)
                && model == frozen.model
                && model_profile == frozen.profile
            {
                snapshot = frozen.messages;
                wire_tools = frozen.tools;
                available_tool_names = frozen.available_tool_names;
                skip_global_cache_for_system_prompt = frozen.skip_global_cache;
                system_override = Some(frozen.system);
            } else {
                loop_state.recovery.max_output_tokens_override = None;
            }
        }
        let system_prompt = system_override.as_ref().unwrap_or(system_prompt);
        let max_output_budget = loop_state.recovery.max_output_tokens_override.take();
        let retry_generation_allowed = !orch
            .hooks
            .has_hooks_for(&hooks::events::HookEventType::MessageDisplay)
            .await;
        let tool_declarations = llm_runtime::convert::to_tool_declarations(wire_tools.clone())
            .map_err(OrchestratorError::ApiCall)?;
        let mut query_history = snapshot.clone();

        let mut exec = crate::streaming_executor::StreamingToolExecutor::try_new_for_query(
            orch,
            query_history.clone(),
            loop_state.native_query_depth,
            user_cancel.clone(),
            available_tool_names.clone(),
        )
        .await?;
        // Hold `tool_result` frames until the collection point below can
        // release them in RECEIVED order, with a cancelled tool's synthetic
        // already substituted. Off again after the drive, so any later
        // emission on this orchestrator goes straight out.
        orch.set_tool_frame_buffering(true).await;

        // #5: wall-clock from stream-open through pump completion (incl. any
        // 529→non-stream fallback) so the CostTracker records a REAL duration
        // instead of `Duration::ZERO`. Paired with
        // `orch.streaming_api.last_retry_count()` at the billing site below.
        let api_call_started = std::time::Instant::now();
        let stream_started = tokio::time::Instant::now();

        // tengu_api_success `messageCount:n` / `messageTokens:r`: capture from
        // the OUTGOING snapshot BEFORE it is moved into `.stream(...)`.
        let api_success_message_count = u32::try_from(snapshot.len()).unwrap_or(u32::MAX);
        let api_success_message_tokens = compaction::grouping::estimate_tokens_for_range(&snapshot);
        let mut did_fall_back_to_non_streaming = false;

        orch.sync_thinking_signature_strip_flag_to_api().await;
        if let Some(scope) = cost_scope.as_ref() {
            scope.preflight().await.map_err(|error| {
                OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
            })?;
        }
        let mut cost_receipt = None;
        let output_observation = orch.capture_main_output().await?;
        let mut first_output_observation = Some(output_observation);
        // Either an open stream to pump, or a turn already RECOVERED from a
        // connect-phase prompt-too-long (#1, see the ContextOverflow arm).
        let mod_host = if let Some(registry) = &orch.lifecycle_runtime.hook_registry {
            registry
                .read()
                .await
                .mod_host()
                .filter(|host| host.has_event("turn.step"))
        } else {
            None
        };
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut snapshot,
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
        );
        // This is the last async-hook generation fence before the physical
        // request; cache persistence and later tool dispatch use this exact
        // projection rather than the earlier pre-fence clone.
        query_history.clone_from(&snapshot);
        let typed_attachment_ids = orch.typed_attachment_ids_for_snapshot(&snapshot);
        let task_deliveries = orch.task_deliveries_for_snapshot(&snapshot).await;
        let turn_step_hook_active = mod_host.is_some();
        if turn_step_hook_active || !retry_generation_allowed {
            loop_state.recovery.last_generation = None;
        } else {
            // Freeze the exact post-hook generation that will be dispatched.
            // This keeps source attachments, request permissions and UPE rows
            // aligned with a budget retry instead of capturing a pre-hook view.
            loop_state.recovery.last_generation = Some(crate::output_recovery::FrozenGeneration {
                model: model.clone(),
                profile: model_profile.clone(),
                system: system_prompt.clone(),
                messages: snapshot.clone(),
                tools: wire_tools.clone(),
                available_tool_names: available_tool_names.clone(),
                skip_global_cache: skip_global_cache_for_system_prompt,
                thinking_resumption_host_facts: thinking_resumption_host_facts.clone(),
            });
        }
        if turn_step_hook_active || !retry_generation_allowed {
            loop_state.recovery.resume_incomplete_thinking_next_request = None;
        } else {
            loop_state.recovery.apply_native_thinking_resume_intent(
                &model,
                model_profile.as_deref(),
                &mut native_anthropic_request_context,
            );
        }
        // Best-effort Mod callers can swallow their local error. Check the
        // scoped admission result synchronously before any physical model
        // request or spawned turn.step worker is admitted.
        ensure_mod_recovery_accepted()?;
        let mut mod_model_state = None;
        let stream_result = if let Some(host) = mod_host {
            let mut input = serde_json::json!({
                "turnId":turn_id,
                "index":loop_state.turn_count.saturating_sub(1),
                "model":model,
                "messageCount":snapshot.len(),
            });
            if let Some(effort) = orch
                .model_runtime
                .current_effort
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                input["effort"] = serde_json::json!(effort);
            }
            // `turn.step` runs in a spawned task via a separate stream-dispatch
            // path, so admit its side effects before spawning. An admission
            // failure is a turn failure, not a Mod error eligible for provider
            // fallback.
            let recovery_leases = orch
                .capture_tool_dispatch_recovery_guard()
                .map(|admission| admission())
                .transpose()
                .map_err(|error| match error {
                    hooks::RecoveryAdmissionError::Interrupted => {
                        OrchestratorError::RecoveryInterrupted
                    }
                    hooks::RecoveryAdmissionError::Failure(error) => {
                        OrchestratorError::Internal(error)
                    }
                })?
                .flatten();
            let session = host
                .capture_turn_step_session()
                .await
                .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
            let (mut stream, effective_model) = mod_turn_step_stream(
                host,
                session,
                recovery_leases,
                orch.streaming_api.clone(),
                orch.output.clone(),
                orch.api.clone(),
                orch.mod_settings_reader.clone(),
                orch.config.model_catalog_restriction.clone(),
                orch.config.server_fallback_regular_model_overrides.clone(),
                orch.config.model.clone(),
                input,
                model_profile.clone(),
                orch.main_model_source_for_request(&model, model_profile.as_deref())
                    .await,
                system_prompt.clone(),
                snapshot,
                guarded_async_hook_reminders.clone(),
                tool_declarations.clone(),
                tool_result_reminder_suffixes.clone(),
                skip_global_cache_for_system_prompt,
                crate::config::sanitize_query_source(&orch.config.query_source).to_owned(),
                first_output_observation.take().flatten(),
                retry_scope.clone(),
                orch.model_runtime.prompt_cache_capture.clone(),
                orch.turn_request_dispatch_observer(),
                native_anthropic_request_context.clone(),
                typed_attachment_ids.clone(),
                task_deliveries.clone(),
            );
            mod_model_state = Some(effective_model.clone());
            // Poll one item before returning so connect-phase provider errors
            // still reach prompt-too-long and model-fallback handling below.
            let mut cancelled_before_first = false;
            let first = if let Some(cancel) = user_cancel {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        cancelled_before_first = true;
                        None
                    }
                    first = stream.next() => first,
                }
            } else {
                stream.next().await
            };
            if let Some((forwarded_model, forwarded_profile)) =
                effective_model.lock().unwrap().clone()
            {
                model = forwarded_model;
                model_profile = forwarded_profile;
            }
            crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                model: model.clone(),
                profile: model_profile.clone(),
            });
            if cancelled_before_first {
                // Drop the consumer so the detached Mod worker receives the
                // same cancellation instead of remaining parked on a pending
                // provider stream after the foreground turn has stopped.
                drop(stream);
                Ok(futures::stream::empty().boxed())
            } else {
                match first {
                    Some(Ok(event)) => Ok(futures::stream::once(async move { Ok(event) })
                        .chain(stream)
                        .boxed()),
                    Some(Err(error)) => Err(error),
                    None => Ok(stream),
                }
            }
        } else {
            crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                model: model.clone(),
                profile: model_profile.clone(),
            });
            crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                &mut snapshot,
                &mut turn_reminders,
                &mut guarded_async_hook_reminders,
            );
            let request_dispatch_admission =
                crate::prompt::async_hook_response::request_dispatch_admission(
                    &snapshot,
                    &guarded_async_hook_reminders,
                );
            let request_dispatch_observer =
                observe_main_dispatch(orch.turn_request_dispatch_observer(), &orch.output);
            orch.cancel_idle_compaction();
            open_cancelable_stream(
                user_cancel,
                Box::pin(
                    retry_scope.run(
                        orch.model_runtime.prompt_cache_capture.scope(
                            orch.streaming_api.stream_with_effort_override(
                                &model,
                                model_profile.as_deref(),
                                system_prompt.as_ref(),
                                snapshot,
                                tool_declarations.clone(),
                                max_output_budget,
                                None,
                                tool_result_reminder_suffixes.clone(),
                                crate::config::sanitize_query_source(&orch.config.query_source),
                                skip_global_cache_for_system_prompt,
                                request_dispatch_admission,
                                request_dispatch_observer,
                                native_anthropic_request_context.clone(),
                                typed_attachment_ids,
                                task_deliveries,
                                orch.main_model_source_for_request(
                                    &model,
                                    model_profile.as_deref(),
                                )
                                .await,
                            ),
                        ),
                    ),
                ),
            )
            .await
            .map(|stream| {
                output_accounting_impl::account_stream(
                    stream,
                    first_output_observation.take().flatten(),
                )
            })
        };
        orch.record_native_afk_beta_rejection_if_any();
        orch.persist_thinking_signature_strip_latch().await;
        // Cancellation can arrive while the HTTP connection is still opening.
        // Its terminal result must not turn a closed/empty response into an
        // API-error assistant row before the pump's cancellation checkpoint.
        if token_aborted(user_cancel) {
            drop(stream_result);
            let cost = orch.snapshot_cost_real().await;
            orch.emit_turn_terminal("aborted_streaming", &cost).await;
            orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                .await;
            return Ok(OpenStreamingOutcome::Complete(assistant_id));
        }
        if stream_result.is_err() {
            if let Some(reason) = retry_scope.take_model_fallback_request() {
                Self::advance_model(orch, loop_state, &model, model_profile.as_deref(), reason)
                    .await?;
                return Ok(OpenStreamingOutcome::ModelFallback);
            }
        }
        let opened = match stream_result {
            Ok(s) => OpenedModelStream::Stream(s),
            // Native retries an HTTP model-not-found response once without
            // streaming on the same model. A local registry miss has no HTTP
            // facts and must never dispatch this fallback.
            Err(error)
                if error.native_anthropic_error_facts().is_some()
                    && error.http_status() == Some(404)
                    && !is_env_truthy(
                        std::env::var(branding::DISABLE_NONSTREAMING_FALLBACK_ENV)
                            .as_deref()
                            .ok(),
                    )
                    && retry_scope.take_non_streaming_fallback() =>
            {
                did_fall_back_to_non_streaming = true;
                let fallback_result = Self::fallback_after_stream_error(
                    orch,
                    OrchestratorError::Streaming(error),
                    outgoing_history_rewriter.as_ref(),
                    deferred_reminder.as_ref(),
                    date_change_reminder.as_ref(),
                    &mut turn_reminders,
                    &mut guarded_async_hook_reminders,
                    &context_announcements,
                    &wire_tools,
                    available_tool_names.clone(),
                    skip_global_cache_for_system_prompt,
                    native_anthropic_request_context.clone(),
                    system_prompt,
                    user_cancel,
                    display_hook_active,
                    loop_state.native_query_depth,
                    assistant_id,
                    cost_scope.as_ref(),
                    api_call_started,
                    retry_scope,
                    false,
                )
                .await;
                orch.record_native_afk_beta_rejection_if_any();
                match fallback_result {
                    Ok(StreamErrorFallback::Recovered {
                        pumped,
                        executor,
                        request_history,
                        cost_receipt: fallback_cost_receipt,
                    }) => {
                        exec = executor;
                        query_history = request_history;
                        cost_receipt = fallback_cost_receipt;
                        OpenedModelStream::Recovered(pumped)
                    }
                    Ok(StreamErrorFallback::Declined(message_id)) => {
                        return Ok(OpenStreamingOutcome::Complete(message_id));
                    }
                    Ok(StreamErrorFallback::Cancelled) => {
                        let cost = orch.snapshot_cost_real().await;
                        orch.emit_turn_terminal("aborted_streaming", &cost).await;
                        orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                            .await;
                        return Ok(OpenStreamingOutcome::Complete(assistant_id));
                    }
                    Err(error) if crate::turn_loop::is_carveout_propagated(&error) => {
                        return Err(error);
                    }
                    Err(error) => {
                        let env = classify_api_error(&error);
                        let text = crate::turn_loop::orchestrator_error_text_source(&error, &model);
                        let provider_status = match &error {
                            OrchestratorError::ApiCall(inner)
                            | OrchestratorError::Streaming(inner)
                                if !matches!(
                                    inner.classification_source(),
                                    LlmError::RequestTooLarge
                                ) =>
                            {
                                inner.http_status()
                            }
                            _ => None,
                        };
                        orch.output
                            .note_model_error_kind(crate::turn_loop::model_error_kind(&error));
                        let message_id =
                            crate::turn_loop::surface_model_error(orch, text, env, provider_status)
                                .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.mark_mod_turn_error();
                        orch.emit_turn_terminal("model_error", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(message_id));
                    }
                }
            }
            // #1 (main-loop parity): a connect-phase 413 / prompt-too-long
            // surfaces HERE as `LlmError::ContextOverflow` — the adapter's
            // `drive_stream` returns `Err` on connect status >= 400, so it
            // never reaches the pump. The proactive blocking-limit preempt
            // above undershot the server's own limit, so recover REACTIVELY
            // via the SAME helper the batched path uses
            // (`call_api_with_ptl_recovery`): summarize while preserving the
            // original history, then retry. Local application input budgets
            // use this same recovery without inventing provider overflow. On
            // success we replay the recovered
            // non-streaming response exactly like the 529 fallback below; on
            // exhaustion we end the turn with the byte-exact prompt_too_long /
            // rapid_refill copy — identical to the proactive preempt and the
            // batched path. NOT gated on
            // `LINGXI_DISABLE_NONSTREAMING_FALLBACK` (that flag governs
            // the 529 overload fallback; PTL recovery is the always-on batched
            // behavior). Previously a streaming 413 bubbled as a hard
            // `OrchestratorError::Streaming` error (documented divergence).
            Err(error) if error.context_recovery_token_gap().is_some() => {
                // Re-snapshot history — the per-turn `snapshot` was MOVED into
                // the failed `stream()` call. Prepend the additional-context
                // meta message, like every `callModel` (claude-code `A6n`).
                let (recov_snapshot_raw, recov_model, recov_profile) = {
                    let s = orch.session.lock().await;
                    (
                        s.model_context_history(),
                        s.model.clone(),
                        s.model_profile.clone(),
                    )
                };
                let mut recov_snapshot = orch
                    .rewrite_outgoing_history(
                        recov_snapshot_raw,
                        outgoing_history_rewriter.as_ref(),
                    )
                    .await?;
                orch.reattach_outgoing_context(
                    &mut recov_snapshot,
                    deferred_reminder.as_ref(),
                    date_change_reminder.as_ref(),
                    &mut turn_reminders,
                    &mut guarded_async_hook_reminders,
                    &context_announcements,
                    false,
                )
                .await;
                crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                    model: recov_model.clone(),
                    profile: recov_profile.clone(),
                });
                let recovered_result = call_api_with_ptl_recovery(
                    orch,
                    system_prompt.as_ref(),
                    skip_global_cache_for_system_prompt,
                    &recov_model,
                    recov_profile.as_deref(),
                    recov_snapshot,
                    outgoing_history_rewriter.clone(),
                    wire_tools.clone(),
                    None,
                    deferred_reminder.clone(),
                    date_change_reminder.clone(),
                    turn_reminders.clone(),
                    tool_result_reminder_suffixes.clone(),
                    &mut guarded_async_hook_reminders,
                    &context_announcements,
                    cost_scope.as_ref(),
                    native_anthropic_request_context.clone(),
                    user_cancel.as_ref(),
                )
                .await;
                orch.record_native_afk_beta_rejection_if_any();
                let recovered = recovered_result?;
                let mut no_snapshot = Vec::new();
                crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                    &mut no_snapshot,
                    &mut turn_reminders,
                    &mut guarded_async_hook_reminders,
                );
                match recovered {
                    PtlCallOutcome::Response {
                        response: resp,
                        request_history,
                    } => {
                        query_history = request_history.clone();
                        // Replay the recovered non-streaming response exactly
                        // like the 529 fallback below: emit text live, rebuild
                        // a fresh executor, register its tool_uses, and flow on
                        // as the turn's `pumped` result.
                        let mut pumped_from_recovery = llm_response_to_pumped_turn(&resp);
                        if let Some(timing_capture) = orch.active_turn_timing_capture() {
                            timing_capture.discard_unobserved_request_dispatch();
                            pumped_from_recovery.timing_capture = Some(timing_capture);
                        }
                        cost_receipt = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &model,
                            model_profile.as_deref(),
                            &pumped_from_recovery,
                            api_call_started.elapsed(),
                        );
                        orch.check_output_accounting()?;
                        append_recovered_assistant_row(
                            orch,
                            &mut pumped_from_recovery,
                            assistant_id,
                        )
                        .await;

                        // P2-04: when a `MessageDisplay` hook is active the
                        // completed-message pass below is the single on-screen
                        // render (with `displayContent` substitution) — skip the
                        // direct whole-body emit here to avoid double display.
                        if !display_hook_active {
                            for blk in &pumped_from_recovery.assistant_blocks {
                                if let Some(text) = visible_text_projection(blk) {
                                    orch.output.emit_text(&text).await;
                                }
                            }
                        }
                        exec = crate::streaming_executor::StreamingToolExecutor::try_new_for_query(
                            orch,
                            request_history.clone(),
                            loop_state.native_query_depth,
                            user_cancel.clone(),
                            available_tool_names.clone(),
                        )
                        .await?;
                        let assistant_message =
                            pumped_assistant_message(&pumped_from_recovery, assistant_id);
                        let mut prior_tool_uses = Vec::new();
                        for tu in &pumped_from_recovery.tool_uses {
                            exec.add_tool_with_context_owned(
                                tu.id.clone(),
                                tu.name.clone(),
                                tu.input.clone(),
                                tu.provider_id.clone(),
                                assistant_id,
                                crate::turn_loop::ToolUseDispatchFacts {
                                    native_query_depth: loop_state.native_query_depth,
                                    query_history: request_history.clone(),
                                    assistant_message: assistant_message.clone(),
                                    same_turn_tool_uses: prior_tool_uses.clone(),
                                    refusal_call_boundary: None,
                                },
                            )
                            .await?;
                            prior_tool_uses.push(ContentBlock::ToolUse {
                                id: tu.id.clone(),
                                name: tu.name.clone(),
                                input: tu.input.clone(),
                                provider_id: tu.provider_id.clone(),
                            });
                        }
                        OpenedModelStream::Recovered(pumped_from_recovery)
                    }
                    PtlCallOutcome::Cancelled => {
                        let cost = orch.snapshot_cost_real().await;
                        orch.emit_turn_terminal("aborted_streaming", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(assistant_id));
                    }
                    PtlCallOutcome::PromptTooLong => {
                        // Reactive recovery exhausted — end with the reactive
                        // terminal reason `prompt_too_long` (`query.ts:1175`).
                        let id = surface_prompt_too_long(orch).await;
                        let _ = orch
                            .handle_stop_at_end(
                                "prompt_too_long",
                                &mut loop_state.stop_hook_active,
                                &mut loop_state.stop_hook_blocking_count,
                                loop_state.turn_count,
                                id,
                                token_aborted(user_cancel),
                            )
                            .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.mark_mod_turn_error();
                        orch.emit_turn_terminal("prompt_too_long", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(id));
                    }
                    PtlCallOutcome::BlockingLimit => {
                        // The recovery's own PROACTIVE step-1 preempt fired on
                        // the re-snapshotted history ⇒ terminal `"blocking_limit"`
                        // (distinct from reactive-exhausted `prompt_too_long`),
                        // mirroring the batched path's `BlockingLimit` arm.
                        let id = surface_prompt_too_long(orch).await;
                        let _ = orch
                            .handle_stop_at_end(
                                "blocking_limit",
                                &mut loop_state.stop_hook_active,
                                &mut loop_state.stop_hook_blocking_count,
                                loop_state.turn_count,
                                id,
                                token_aborted(user_cancel),
                            )
                            .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.mark_mod_turn_error();
                        orch.emit_turn_terminal("blocking_limit", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(id));
                    }
                    PtlCallOutcome::RapidRefillBreaker => {
                        // #54 reactive trip — surface the thrashing message and
                        // end with the terminal reason `rapid_refill_breaker`
                        // (the MESSAGE still carries api-error `invalid_request`),
                        // mirroring the batched path.
                        let id = surface_rapid_refill_thrashing(orch).await;
                        let _ = orch
                            .handle_stop_at_end(
                                "rapid_refill_breaker",
                                &mut loop_state.stop_hook_active,
                                &mut loop_state.stop_hook_blocking_count,
                                loop_state.turn_count,
                                id,
                                token_aborted(user_cancel),
                            )
                            .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.mark_mod_turn_error();
                        orch.emit_turn_terminal("rapid_refill_breaker", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(id));
                    }
                }
            }
            // #10: a connect-phase RateLimited/Overloaded keeps its dedicated
            // downstream handling — propagate as a hard error.
            Err(e)
                if matches!(
                    e.classification_source(),
                    LlmError::RateLimited { .. } | LlmError::Overloaded { .. }
                ) =>
            {
                return Err(OrchestratorError::Streaming(e));
            }
            // #10: any other connect-phase model/runtime error ends the turn
            // GRACEFULLY as `model_error` (faithful port of the `query.ts`
            // catch) — surface the raw error text as an api-error assistant
            // message instead of bubbling a hard error. No assistant message
            // was persisted this turn, so no orphaned tool_use to repair.
            Err(other) => {
                // Classify the typed connect-phase error (`Flp`/`KNn`) before
                // consuming it for the verbatim message text. Wrap into the
                // `Streaming` variant — this is the connect-phase streaming
                // surface — so the classifier sees the inner `LlmError`.
                let env = classify_api_error(&OrchestratorError::Streaming(other.clone()));
                let error_text = orch.model_error_source(&other).await;
                orch.output
                    .note_model_error_kind(crate::turn_loop::model_error_kind(
                        &OrchestratorError::Streaming(other.clone()),
                    ));
                let id = crate::turn_loop::surface_model_error(
                    orch,
                    error_text,
                    env,
                    other.http_status(),
                )
                .await;
                let cost = orch.snapshot_cost_real().await;
                orch.mark_mod_turn_error();
                orch.emit_turn_terminal("model_error", &cost).await;
                return Ok(OpenStreamingOutcome::Complete(id));
            }
        };

        Ok(OpenStreamingOutcome::Opened(OpenedStreamingIteration {
            opened,
            exec,
            query_history,
            retry_scope: retry_scope.clone(),
            model,
            model_profile,
            outgoing_history_rewriter,
            turn_reminders,
            tool_result_reminder_suffixes,
            native_anthropic_request_context,
            guarded_async_hook_reminders,
            context_announcements,
            wire_tools,
            available_tool_names,
            skip_global_cache_for_system_prompt,
            deferred_reminder,
            date_change_reminder,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            turn_step_hook_active,
            mod_model_state,
            api_call_started,
            stream_started,
            api_success_message_count,
            api_success_message_tokens,
            did_fall_back_to_non_streaming,
            cost_scope,
            cost_receipt,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn fallback_after_stream_error<'a>(
        orch: &'a ConversationOrchestrator,
        error: OrchestratorError,
        outgoing_history_rewriter: Option<&Arc<dyn OutgoingHistoryRewriter>>,
        deferred_reminder: Option<&ConversationMessage>,
        date_change_reminder: Option<&ConversationMessage>,
        turn_reminders: &mut Vec<ConversationMessage>,
        guarded_async_hook_reminders: &mut Vec<(MessageId, Arc<dyn HookPublicationGuard>)>,
        context_announcements: &PreparedContextAnnouncements,
        wire_tools: &[lingxi_core::host::SourceToolWireDefinition],
        available_tool_names: Option<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
        skip_global_cache_for_system_prompt: bool,
        native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        display_hook_active: bool,
        native_query_depth: u32,
        assistant_id: MessageId,
        cost_scope: Option<&cost::CostSessionScope>,
        api_call_started: std::time::Instant,
        retry_scope: &ModelCallRetryScope,
        failed_stream_outlasted_timeout: bool,
    ) -> Result<StreamErrorFallback<'a>, OrchestratorError> {
        // Seed: a streaming overload counts as 1 toward the consecutive
        // 529 budget (LlmError::Overloaded = 529).  Other in-band errors
        // (e.g. ProviderInternal) seed 0 — matching TS
        // `is529Error(streamingError) ? 1 : 0` (claude.ts:2559).
        let seed: u8 = u8::from(matches!(
            &error,
            OrchestratorError::Streaming(error)
                if matches!(error.classification_source(), LlmError::Overloaded { .. })
        ));
        // The current stream decision already recorded this body overload in
        // the shared ledger. The seed initializes only the nonstream driver's
        // local view; configure() merges it with the existing count.

        // Re-snapshot history for the non-streaming call (the partial
        // stream never touched session.history, so it is still the same
        // snapshot we used for the stream — no reset needed).
        let (non_stream_snapshot_raw, mut non_stream_model, mut non_stream_profile) = {
            let s = orch.session.lock().await;
            (
                s.model_context_history(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        if let Some(route) = crate::query_model::current() {
            non_stream_model = route.model;
            non_stream_profile = route.profile;
        }
        let mut non_stream_snapshot = orch
            .rewrite_outgoing_history(non_stream_snapshot_raw, outgoing_history_rewriter)
            .await?;
        // R-P1c/R-P1d: claude-code's `A6n` prepends the additional-
        // context meta message on EVERY `callModel`, including this
        // non-streaming fallback. Prepend it to the re-snapshot too.
        orch.reattach_outgoing_context(
            &mut non_stream_snapshot,
            deferred_reminder,
            date_change_reminder,
            turn_reminders,
            guarded_async_hook_reminders,
            context_announcements,
            false,
        )
        .await;
        let tools_for_fallback = llm_runtime::convert::to_tool_declarations(wire_tools.to_vec())
            .map_err(OrchestratorError::ApiCall)?;

        if let Some(scope) = cost_scope {
            scope.preflight().await.map_err(|error| {
                OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
            })?;
        }

        let mut output_observation = orch.capture_main_output().await?;
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut non_stream_snapshot,
            turn_reminders,
            guarded_async_hook_reminders,
        );
        let request_dispatch_admission =
            crate::prompt::async_hook_response::request_dispatch_admission(
                &non_stream_snapshot,
                guarded_async_hook_reminders,
            );
        crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
            model: non_stream_model.clone(),
            profile: non_stream_profile.clone(),
        });
        let request_history = non_stream_snapshot.clone();
        let resp = side_model_request(
            orch,
            user_cancel,
            retry_scope.run(
                orch.model_runtime
                    .prompt_cache_capture
                    .scope(
                        orch.api
                            .messages_create(crate::OrchestratorApiRequest::Main({
                                let mut request = llm_runtime::MessagesCreateRequest::new(
                                    &non_stream_model,
                                    non_stream_profile.as_deref(),
                                    system_prompt.as_ref().cloned(),
                                    non_stream_snapshot,
                                    tools_for_fallback,
                                );
                                request.opts.skip_global_cache_for_system_prompt =
                                    skip_global_cache_for_system_prompt;
                                request.opts.native_anthropic_request_context =
                                    native_anthropic_request_context.clone();
                                request.opts.typed_attachment_ids =
                                    orch.typed_attachment_ids_for_snapshot(&request.messages);
                                request.opts.task_deliveries =
                                    orch.task_deliveries_for_snapshot(&request.messages).await;
                                request.opts.request_dispatch_admission =
                                    request_dispatch_admission;
                                request.opts.request_dispatch_observer =
                                    observe_main_dispatch(None, &orch.output);
                                request.opts.query_source = Some(
                                    crate::config::sanitize_query_source(&orch.config.query_source)
                                        .to_string(),
                                );
                                request.opts.model_source = orch
                                    .main_model_source_for_request(
                                        &non_stream_model,
                                        non_stream_profile.as_deref(),
                                    )
                                    .await;
                                request.opts.initial_consecutive_overloaded = Some(seed);
                                request.opts.failed_stream_outlasted_timeout =
                                    failed_stream_outlasted_timeout;
                                request.opts.fallback = orch
                                    .config
                                    .fallback_model
                                    .as_deref()
                                    .map(llm_runtime::FallbackPolicy::from_models_csv)
                                    .unwrap_or(llm_runtime::FallbackPolicy::Disabled);
                                request
                            })),
                    ),
            ),
        )
        .await;
        let Some(resp_result) = resp else {
            return Ok(StreamErrorFallback::Cancelled);
        };
        let mut resp = resp_result.map_err(OrchestratorError::ApiCall)?;

        if let Some(observation) = &mut output_observation {
            observation.observe(&resp.usage);
            let _ = observation.finish();
        }

        let server_fallback_events = resp.server_fallback_events();
        let mut handled_server_fallback_events = 0;
        for info in &server_fallback_events {
            if !matches!(info.event.reason.as_str(), "refusal" | "sticky") {
                continue;
            }
            match crate::server_fallback::handle(orch, info, false).await? {
                crate::server_fallback::ServerFallbackAdmission::Applied => {
                    handled_server_fallback_events += 1;
                    if matches!(info.event.reason.as_str(), "refusal" | "sticky") {
                        resp.model =
                            lingxi_core::host::refusal_server_control::resolve_received_model(
                                Some(&info.lane.model),
                                &info.event.to_model,
                            );
                    }
                }
                crate::server_fallback::ServerFallbackAdmission::Declined => {
                    let rejected = llm_response_to_pumped_turn(&resp);
                    if let Some(receipt) = Self::begin_stream_cost_response(
                        orch,
                        cost_scope,
                        &resp.model,
                        non_stream_profile.as_deref(),
                        &rejected,
                        api_call_started.elapsed(),
                    ) {
                        let settlement = receipt.settle().await;
                        orch.model_runtime
                            .api_calls_recorded
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if let Err(error) = settlement.persistence_result() {
                            orch.note_cost_settlement_failure(error).await;
                        }
                    }
                    orch.record_prompt_cache_usage(&resp.usage).await;
                    crate::server_fallback::flush_pending_notice(orch).await;
                    let (message_id, _) =
                        crate::server_fallback::surface_declined(orch, info, Some(&resp.id)).await;
                    return Ok(StreamErrorFallback::Declined(message_id));
                }
            }
        }

        // Convert HistoryResponse → PumpedTurn so the rest of the streaming
        // turn loop can proceed identically.
        let mut pumped_from_fallback = llm_response_to_pumped_turn(&resp);
        if let Some(timing_capture) = orch.active_turn_timing_capture() {
            timing_capture.discard_unobserved_request_dispatch();
            pumped_from_fallback.timing_capture = Some(timing_capture);
        }
        pumped_from_fallback.handled_server_fallback_events = handled_server_fallback_events;
        let cost_receipt = Self::begin_stream_cost_response(
            orch,
            cost_scope,
            &non_stream_model,
            non_stream_profile.as_deref(),
            &pumped_from_fallback,
            api_call_started.elapsed(),
        );

        orch.check_output_accounting()?;
        append_recovered_assistant_row(orch, &mut pumped_from_fallback, assistant_id).await;

        // Emit text blocks from the non-streaming response to the output
        // stream, mirroring the batched path (turn_loop.rs step 4:
        // `orch.output.emit_text(text, units).await`).  In the normal streaming
        // path `pump_stream` calls `dispatch_event` → `emit_text` for each
        // `TextDelta`; the non-streaming path has no SSE events, so we
        // replicate the whole-body emit here.
        // P2-04: suppressed when a `MessageDisplay` hook is active — the
        // completed-message pass renders the (possibly substituted) text
        // once, so skip the direct emit to avoid double display.
        for blk in &pumped_from_fallback.assistant_blocks {
            if let Some(text) = visible_text_projection(blk) {
                if !display_hook_active {
                    orch.output.emit_text(&text).await;
                }
                continue;
            }
            match blk {
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    orch.output
                        .emit_thinking(thinking, signature.as_deref())
                        .await;
                }
                ContentBlock::RedactedThinking { data } => {
                    orch.output.emit_redacted_thinking(data).await;
                }
                _ => {}
            }
        }

        // claude-code `query.ts:733-740`: discard the partial
        // streaming attempt's executor (its tool_uses have stale ids
        // and would orphan against the fallback response) and replace
        // it with a fresh one. Dropping the old executor cancels any
        // in-flight tool futures it had started mid-stream. The fresh
        // executor's tools are registered from the FALLBACK response's
        // tool_uses by the post-stream drive loop below (this is the
        // ONLY path that still `add_tool`s after the stream — the
        // normal path registers mid-stream).
        let mut exec = crate::streaming_executor::StreamingToolExecutor::try_new_for_query(
            orch,
            request_history.clone(),
            native_query_depth,
            user_cancel.clone(),
            available_tool_names.clone(),
        )
        .await?;
        let assistant_message = pumped_assistant_message(&pumped_from_fallback, assistant_id);
        let mut prior_tool_uses = Vec::new();
        for tu in &pumped_from_fallback.tool_uses {
            exec.add_tool_with_context_owned(
                tu.id.clone(),
                tu.name.clone(),
                tu.input.clone(),
                tu.provider_id.clone(),
                assistant_id,
                crate::turn_loop::ToolUseDispatchFacts {
                    native_query_depth: exec.native_query_depth(),
                    query_history: request_history.clone(),
                    assistant_message: assistant_message.clone(),
                    same_turn_tool_uses: prior_tool_uses.clone(),
                    refusal_call_boundary: None,
                },
            )
            .await?;
            prior_tool_uses.push(ContentBlock::ToolUse {
                id: tu.id.clone(),
                name: tu.name.clone(),
                input: tu.input.clone(),
                provider_id: tu.provider_id.clone(),
            });
        }

        Ok(StreamErrorFallback::Recovered {
            pumped: pumped_from_fallback,
            executor: exec,
            request_history,
            cost_receipt,
        })
    }

    fn pump_iteration<'orch: 'call, 'call>(
        orch: &'orch ConversationOrchestrator,
        prepared: PreparedStreamingIteration,
        system_prompt: &'call Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &'call Option<CancellationToken>,
        loop_state: &'call mut TurnLoopState,
    ) -> BoxFuture<'call, Result<PumpStreamingOutcome<'orch>, OrchestratorError>> {
        Box::pin(StreamingTurnDriver::<'orch>::pump_iteration_inner(
            orch,
            prepared,
            system_prompt,
            user_cancel,
            loop_state,
        ))
    }

    async fn pump_iteration_inner<'a>(
        orch: &'a ConversationOrchestrator,
        prepared: PreparedStreamingIteration,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
    ) -> Result<PumpStreamingOutcome<'a>, OrchestratorError> {
        let primary = loop_state
            .selected_route
            .as_ref()
            .map_or(prepared.step.model.as_str(), |route| route.model.as_str());
        let has_fallback = orch.config.fallback_model.as_deref().is_some_and(|models| {
            crate::query_model::configured_chain(models, primary).len() > loop_state.fallback_index
        });
        let retry_scope = if has_fallback {
            ModelCallRetryScope::default().with_model_fallback()
        } else {
            ModelCallRetryScope::default()
        };
        let opened = match Self::open_iteration(
            orch,
            prepared,
            system_prompt,
            user_cancel,
            loop_state,
            &retry_scope,
        )
        .await?
        {
            OpenStreamingOutcome::Opened(iteration) => iteration,
            OpenStreamingOutcome::Complete(message_id) => {
                return Ok(PumpStreamingOutcome::Complete(message_id));
            }
            OpenStreamingOutcome::ModelFallback => return Ok(PumpStreamingOutcome::ModelFallback),
        };
        Self::pump_opened_iteration(
            orch,
            opened,
            system_prompt,
            user_cancel,
            loop_state,
            retry_scope,
        )
        .await
    }

    async fn pump_opened_iteration<'a>(
        orch: &'a ConversationOrchestrator,
        opened_iteration: OpenedStreamingIteration<'a>,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
        retry_scope: ModelCallRetryScope,
    ) -> Result<PumpStreamingOutcome<'a>, OrchestratorError> {
        let OpenedStreamingIteration {
            opened,
            mut exec,
            query_history,
            model,
            model_profile,
            outgoing_history_rewriter,
            mut turn_reminders,
            tool_result_reminder_suffixes,
            native_anthropic_request_context,
            mut guarded_async_hook_reminders,
            context_announcements,
            wire_tools,
            available_tool_names,
            skip_global_cache_for_system_prompt,
            deferred_reminder,
            date_change_reminder,
            assistant_id,
            mut partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            turn_step_hook_active,
            mod_model_state,
            api_call_started,
            stream_started,
            api_success_message_count,
            api_success_message_tokens,
            mut did_fall_back_to_non_streaming,
            cost_scope,
            mut cost_receipt,
            retry_scope: _,
        } = opened_iteration;
        let mut cache_request_history = query_history.clone();
        let cache_inline_context = context_announcements.inline_context.clone();
        let mut assistant_session_append_dispatched =
            matches!(&opened, OpenedModelStream::Recovered(_));
        // Polling the stream can start tool execution immediately. Snapshot
        // the live MCP count before that point so a tool-triggered registry
        // refresh can be compared after PostToolBatch, as in the oracle.
        let pre_batch_mcp_tool_count = orch.filtered_mcp_tool_count().await;
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        settlement.publication_guard = Some(Arc::new(exec.publication_fence()));

        // 3. Pump the stream (with mid-stream 529 → non-streaming fallback OR
        //    the cc 2.1.199 partial-finalize, whichever applies).
        //
        // Task 7 / claude.ts parity: if the stream errors with `LlmError::Overloaded`
        // after the first event — AND `LINGXI_DISABLE_NONSTREAMING_FALLBACK` is not
        // set — AND no content block completed yet — issue a fresh non-streaming call
        // seeded with `initial_consecutive_overloaded = 1`.  This mirrors
        // `claude.ts:2469-2594` + `withRetry.ts:186` (`initialConsecutive529Errors`).
        //
        // The env gate name is locked byte-for-byte to TS:
        //   `process.env.LINGXI_DISABLE_NONSTREAMING_FALLBACK` (claude.ts:2470)
        // Truthiness follows `isEnvTruthy` (non-empty, non-"false", non-"0").
        //
        // P1-04 (cc 2.1.199, binary-verified): once a REAL content block has
        // completed — or a transport close leaves visible text whose stop
        // frame was lost — the partial is no longer discarded. The
        // `partial_has_output` arm in the `match pump_outcome` below finalizes
        // it in place (synthesized stop_reason + usage + `tengu_streaming_partial_finalized`),
        // persists the streamed blocks, and appends the "API Error: … may be
        // incomplete." notice — instead of the pre-2.1.199 discard-and-refetch.
        // The non-streaming fallback therefore fires only when the stream
        // produced no recoverable output. Provider errors before the first
        // completed block retain cc's `_r`-length behavior.
        // In both cases partial deltas already reached callers LIVE via
        // `event_router.rs` → `output.emit_text` at each `TextDelta`
        // (claude.ts:2210 `yield m`).
        // #1: a connect-phase prompt-too-long already recovered above (its
        // recovered non-streaming response was replayed) skips the pump; an
        // open stream is pumped as before.
        let (pumped, remaining) = match opened {
            OpenedModelStream::Recovered(pumped_from_recovery) => (pumped_from_recovery, None),
            OpenedModelStream::Stream(first_stream) => {
                // cc 2.1.198 mid-response transient retry (`query.ts` stream
                // loop @219649648): on a transient network drop (ECONNRESET /
                // connection closed / reset) OR a watchdog idle-timeout, re-open
                // and re-pump the SAME streaming request with backoff — but ONLY
                // while `!real_content_started` (binary `!Hr`). Because a
                // `tool_use` block STARTING flips `real_content_started`, this
                // guard also guarantees NO tool has been dispatched, so a
                // non-idempotent tool is never re-run. The failed pump left
                // `session.history` untouched and (by the guard) the executor
                // clean, so the retry reuses `exec` and re-snapshots history.
                let mut cur_stream = first_stream;
                let mut mid_stream_retries: u32 = 0;
                let mut stream_decision;
                let mut stream_retry;
                let mut stream_failed_long;
                let mut websocket_lost;
                let pump_outcome: Result<
                    (
                        crate::streaming_loop::PumpedTurn,
                        futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
                    ),
                    crate::streaming_loop::PumpFailure,
                > = loop {
                    let mut observed_pump =
                        crate::streaming_loop::pump_stream_with_executor_tracked_remaining(
                            cur_stream,
                            &orch.output,
                            ExecutorPump {
                                executor: &mut exec,
                                assistant_id,
                                query_history: query_history.clone(),
                                model_profile: model_profile.clone(),
                                record_supersedes:
                                    crate::streaming_loop::native_server_fallback_supersedes_enabled(
                                        &orch.config.query_source,
                                    ),
                                user_cancel: user_cancel.as_ref(),
                                suppress_live_text: display_hook_active,
                                suppress_live_thinking: turn_step_hook_active,
                                settlement: Some(&mut settlement),
                            },
                        )
                        .await;
                    orch.record_native_afk_beta_rejection_if_any();
                    if let Err(error) = orch.check_output_accounting() {
                        let retained = match &observed_pump {
                            Ok((pumped, _)) => pumped,
                            Err(failure) => &failure.partial,
                        };
                        // The error must not discard known paid usage. This
                        // synchronous handoff survives dropping its waiter.
                        let _receipt = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &model,
                            model_profile.as_deref(),
                            retained,
                            api_call_started.elapsed(),
                        );
                        let already_declined = matches!(
                            &observed_pump,
                            Err(failure)
                                if failure.disposition
                                    == crate::streaming_loop::PumpFailureDisposition::ServerFallbackDeclined
                        );
                        if !already_declined {
                            let partial = match &mut observed_pump {
                                Ok((partial, _)) => partial,
                                Err(failure) => {
                                    failure.partial_close.close(orch.output.as_ref()).await;
                                    &mut failure.partial
                                }
                            };
                            orch.settle_stream_host_failure_attempt(
                                &mut exec,
                                partial,
                                &mut settlement,
                                assistant_id,
                            )
                            .await;
                        }
                        return Err(error);
                    }
                    stream_decision = None;
                    stream_retry = None;
                    stream_failed_long = false;
                    if let Err(failure) = &observed_pump {
                        let cause = crate::streaming_loop::stream_failure_cause(&failure.error);
                        stream_failed_long =
                            llm_runtime::model::request_timeout::stream_outlasted_nonstream_timeout(
                                stream_started.elapsed(),
                            );
                        if failure.progress
                            == llm_runtime::model::stream_recovery::Progress::ThinkingOnly
                            || matches!(
                                cause,
                                Some(
                                    llm_runtime::model::stream_recovery::Cause::ServerError
                                        | llm_runtime::model::stream_recovery::Cause::TimedOut
                                )
                            )
                        {
                            stream_decision = cause.map(|cause| {
                                let outcome = retry_scope.decide_stream_failure(
                                    llm_runtime::model::stream_recovery::Failure {
                                        cause,
                                        progress: failure.progress,
                                        stop_reason_received: failure.partial.stop_reason.is_some(),
                                        outlasted_non_streaming_timeout: stream_failed_long,
                                    },
                                    orch.config.fallback_model.as_deref().is_some_and(|models| {
                                        crate::query_model::configured_chain(
                                            models,
                                            loop_state
                                                .selected_route
                                                .as_ref()
                                                .map_or(model.as_str(), |route| {
                                                    route.model.as_str()
                                                }),
                                        )
                                        .len()
                                            > loop_state.fallback_index
                                    }),
                                    false,
                                    !is_env_truthy(
                                        std::env::var(branding::DISABLE_NONSTREAMING_FALLBACK_ENV)
                                            .ok()
                                            .as_deref(),
                                    ),
                                );
                                stream_retry = Some((cause, outcome.counts));
                                outcome.decision
                            });
                        }
                    }
                    // A WebSocket the server closed under the response keeps
                    // Codex's retry budget after the native one is spent. Only
                    // thinking can have streamed, so no tool has started.
                    websocket_lost = matches!(
                        &observed_pump,
                        Err(failure)
                            if crate::streaming_loop::is_websocket_connection_lost(&failure.error)
                                && !failure.real_content_started
                                && failure.partial.stop_reason.is_none()
                    );
                    match observed_pump {
                        Ok(p) => break Ok(p),
                        Err(mut f)
                            if stream_decision
                                == Some(llm_runtime::model::stream_recovery::Decision::Retry)
                                || (stream_decision.is_none()
                                    && crate::streaming_loop::is_transient_mid_stream(
                                        &f.error,
                                    )
                                    && !f.real_content_started
                                    && mid_stream_retries
                                        < crate::streaming_loop::mid_stream_retry_cap(
                                            &f.error,
                                        )
                                    && retry_scope.take_stream_retry())
                                || (stream_decision.is_some()
                                    && websocket_lost
                                    && mid_stream_retries
                                        < crate::streaming_loop::WEBSOCKET_STREAM_MAX_RETRIES
                                    && retry_scope.take_stream_retry()) =>
                        {
                            f.partial_close.close(orch.output.as_ref()).await;
                            if let Some(usage) = f.partial.usage.as_ref() {
                                orch.record_prompt_cache_usage(usage).await;
                            }
                            mid_stream_retries += 1;
                            // Exponential backoff + jitter (binary `sle`).
                            let delay = match stream_retry {
                                // Codex's ladder for a closed WebSocket: 200ms, doubling.
                                _ if websocket_lost => llm_runtime::model::retry::jittered_delay(
                                    llm_runtime::model::retry::scaled_base_delay_ms(
                                        mid_stream_retries - 1,
                                        Some(crate::streaming_loop::WEBSOCKET_STREAM_RETRY_BASE_MS),
                                    ),
                                ),
                                Some((llm_runtime::model::stream_recovery::Cause::Stalled, _)) => {
                                    std::time::Duration::ZERO
                                }
                                Some((
                                    llm_runtime::model::stream_recovery::Cause::ConnectionLost
                                    | llm_runtime::model::stream_recovery::Cause::Truncated,
                                    counts,
                                )) => std::time::Duration::from_millis(
                                    100 * u64::from(counts.after_thinking_only),
                                ),
                                _ => {
                                    let attempt = stream_retry
                                        .map_or(mid_stream_retries, |(_, counts)| {
                                            counts.after_thinking_only
                                        });
                                    let base = llm_runtime::model::retry::scaled_base_delay_ms(
                                        attempt.saturating_sub(1),
                                        None,
                                    );
                                    llm_runtime::model::retry::jittered_delay(base)
                                }
                            };
                            tokio::time::sleep(delay).await;
                            tracing::warn!(
                                attempt = mid_stream_retries,
                                error = %f.error,
                                "mid-response transient stream error — retrying streaming request"
                            );
                            // Re-snapshot history (+ additional context) for
                            // the retry — same pattern as the 529 fallback.
                            let (re_snapshot_raw, mut re_model, mut re_profile) = {
                                let s = orch.session.lock().await;
                                (
                                    s.model_context_history(),
                                    s.model.clone(),
                                    s.model_profile.clone(),
                                )
                            };
                            if let Some(route) = crate::query_model::current() {
                                re_model = route.model;
                                re_profile = route.profile;
                            }
                            let mut re_snapshot = orch
                                .rewrite_outgoing_history(
                                    re_snapshot_raw,
                                    outgoing_history_rewriter.as_ref(),
                                )
                                .await?;
                            orch.reattach_outgoing_context(
                                &mut re_snapshot,
                                deferred_reminder.as_ref(),
                                date_change_reminder.as_ref(),
                                &mut turn_reminders,
                                &mut guarded_async_hook_reminders,
                                &context_announcements,
                                false,
                            )
                            .await;
                            if let Some(scope) = cost_scope.as_ref() {
                                scope.preflight().await.map_err(|error| {
                                    OrchestratorError::Internal(format!(
                                        "cost durability preflight failed: {error}"
                                    ))
                                })?;
                            }
                            let output_observation = orch.capture_main_output().await?;
                            crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                                &mut re_snapshot,
                                &mut turn_reminders,
                                &mut guarded_async_hook_reminders,
                            );
                            let request_dispatch_admission =
                                crate::prompt::async_hook_response::request_dispatch_admission(
                                    &re_snapshot,
                                    &guarded_async_hook_reminders,
                                );
                            let typed_attachment_ids =
                                orch.typed_attachment_ids_for_snapshot(&re_snapshot);
                            let task_deliveries =
                                orch.task_deliveries_for_snapshot(&re_snapshot).await;
                            let request_dispatch_observer = observe_main_dispatch(
                                orch.turn_request_dispatch_observer(),
                                &orch.output,
                            );
                            orch.cancel_idle_compaction();
                            let retry_stream = open_cancelable_stream(
                                user_cancel,
                                Box::pin(
                                    retry_scope.run(
                                        orch.model_runtime.prompt_cache_capture.scope(
                                            orch.streaming_api.stream_with_effort_override(
                                                &re_model,
                                                re_profile.as_deref(),
                                                system_prompt.as_ref(),
                                                re_snapshot,
                                                llm_runtime::convert::to_tool_declarations(
                                                    wire_tools.clone(),
                                                )
                                                .map_err(OrchestratorError::ApiCall)?,
                                                None,
                                                None,
                                                tool_result_reminder_suffixes.clone(),
                                                crate::config::sanitize_query_source(
                                                    &orch.config.query_source,
                                                ),
                                                skip_global_cache_for_system_prompt,
                                                request_dispatch_admission,
                                                request_dispatch_observer,
                                                native_anthropic_request_context.clone(),
                                                typed_attachment_ids,
                                                task_deliveries,
                                                orch.main_model_source_for_request(
                                                    &re_model,
                                                    re_profile.as_deref(),
                                                )
                                                .await,
                                            ),
                                        ),
                                    ),
                                ),
                            )
                            .await;
                            orch.persist_thinking_signature_strip_latch().await;
                            if retry_stream.is_err() {
                                if let Some(reason) = retry_scope.take_model_fallback_request() {
                                    Self::advance_model(
                                        orch,
                                        loop_state,
                                        &re_model,
                                        re_profile.as_deref(),
                                        reason,
                                    )
                                    .await?;
                                    return Ok(PumpStreamingOutcome::ModelFallback);
                                }
                            }
                            match retry_stream {
                                Ok(s) => {
                                    cur_stream = output_accounting_impl::account_stream(
                                        s,
                                        output_observation,
                                    );
                                    continue;
                                }
                                // Re-open failed: surface as the terminal
                                // pump error for the arms below. No partial to
                                // finalize here — the retry only fires while
                                // `!real_content_started`, so nothing real was
                                // yielded on the attempt we are abandoning.
                                Err(e) => {
                                    break Err(crate::streaming_loop::PumpFailure {
                                        error: OrchestratorError::Streaming(e),
                                        real_content_started: false,
                                        progress:
                                            llm_runtime::model::stream_recovery::Progress::Nothing,
                                        partial_close: Default::default(),
                                        partial: crate::streaming_loop::PumpedTurn::default(),
                                        disposition:
                                            crate::streaming_loop::PumpFailureDisposition::Stream,
                                    });
                                }
                            }
                        }
                        Err(f) => break Err(f),
                    }
                };
                match pump_outcome {
                    // Nothing completed before the interrupt, so there is no
                    // assistant message to keep (Claude Code drops a
                    // thinking-only partial the same way).
                    Ok((p, _))
                        if token_aborted(user_cancel)
                            && !crate::streaming_loop::partial_has_output(&p) =>
                    {
                        return Ok(PumpStreamingOutcome::Interrupted);
                    }
                    Ok((p, tail)) => (p, Some(tail)),
                    Err(mut failure)
                        if failure.disposition
                            == crate::streaming_loop::PumpFailureDisposition::ServerFallbackDeclined =>
                    {
                        failure.partial_close.close(orch.output.as_ref()).await;
                        let info = failure
                            .partial
                            .server_fallback_events
                            .last()
                            .expect("declined pump carries its server fallback observation")
                            .clone();
                        let declined_model =
                            lingxi_core::host::refusal_server_control::resolve_received_model(
                                Some(&info.lane.model),
                                &info.event.to_model,
                            );
                        if let Some(receipt) = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &declined_model,
                            Some(&info.profile),
                            &failure.partial,
                            api_call_started.elapsed(),
                        ) {
                            let settlement = receipt.settle().await;
                            orch.model_runtime
                                .api_calls_recorded
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if let Err(error) = settlement.persistence_result() {
                                orch.note_cost_settlement_failure(error).await;
                            }
                        }
                        if let Some(usage) = failure.partial.usage.as_ref() {
                            orch.record_prompt_cache_usage(usage).await;
                        }
                        crate::server_fallback::flush_pending_notice(orch).await;
                        let (message_id, _) = crate::server_fallback::surface_declined(
                            orch,
                            &info,
                            failure.partial.response_message_id.as_deref(),
                        )
                        .await;
                        return Ok(PumpStreamingOutcome::Complete(message_id));
                    }
                    Err(mut failure)
                        if stream_decision == Some(llm_runtime::model::stream_recovery::Decision::UseFallbackModel) =>
                    {
                        failure.partial_close.close(orch.output.as_ref()).await;
                        // Native `abandonAttempt("chain_advance")` discards the
                        // failed reasoning, closes its frames and rebuilds the
                        // tool executor. Known paid usage belongs to that attempt.
                        if let Some(receipt) = Self::begin_stream_cost_response(
                            orch, cost_scope.as_ref(), &model, model_profile.as_deref(),
                            &failure.partial, api_call_started.elapsed(),
                        ) {
                            let settlement = receipt.settle().await;
                            orch.model_runtime.api_calls_recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if let Err(error) = settlement.persistence_result() {
                                orch.note_cost_settlement_failure(error).await;
                            }
                        }
                        if let Some(usage) = failure.partial.usage.as_ref() {
                            orch.record_prompt_cache_usage(usage).await;
                        }
                        // `abandonAttempt("chain_advance")` disposes the old
                        // executor before yielding any tombstones or rebuilding
                        // the next model attempt.
                        let removal = exec.abandon_without_synthetics().await;
                        if !removal.ids.is_empty() {
                            failure.partial.tool_use_removals.push(removal);
                        }
                        let row_tombstones: Vec<_> = failure
                            .partial
                            .assistant_rows
                            .iter()
                            .map(crate::streaming_loop::tombstone_message_for_row)
                            .collect();
                        let persisted_links: Vec<_> = failure
                            .partial
                            .assistant_rows
                            .iter()
                            .filter_map(|row| row.persisted_link.clone())
                            .collect();
                        for tombstone in &row_tombstones {
                            orch.output
                                .emit_server_fallback_tombstone(tombstone, true)
                                .await;
                        }
                        exec.remove_assistant_stream_rows(&persisted_links).await;
                        let result_tombstones = settlement.discard_attempt_rows();
                        crate::streaming_loop::clear_tombstoned_tool_result_metadata(
                            orch,
                            &result_tombstones,
                        )
                        .await;
                        for tombstone in &result_tombstones {
                            orch.output
                                .emit_server_fallback_tombstone(tombstone, true)
                                .await;
                        }
                        orch.output.emit_message_retracted(&assistant_id).await;
                        Self::advance_model(
                            orch,
                            loop_state,
                            &model,
                            model_profile.as_deref(),
                            if matches!(
                                crate::streaming_loop::stream_failure_cause(&failure.error),
                                Some(llm_runtime::model::stream_recovery::Cause::Overloaded)
                            ) {
                                "overloaded"
                            } else {
                                "server_error"
                            },
                        )
                        .await?;
                        return Ok(PumpStreamingOutcome::ModelFallback);
                    }
                    // P1-04 (cc 2.1.199 partial-stream finalize, binary-verified): a
                    // finalize-class mid-stream error (server/overloaded/api error,
                    // watchdog stall, or connection close) that landed after useful
                    // output is not discarded. The already-streamed
                    // partial is finalized in place — persisted with a synthesized
                    // `stop_reason` (`tool_use` if any tool_use else `end_turn`) +
                    // usage — `tengu_streaming_partial_finalized` fires, and a byte-exact
                    // "API Error: … The response above may be incomplete." notice is
                    // surfaced after it (see the notice + terminal sites below, gated on
                    // `partial_finalize`). This runs BEFORE the 529 non-streaming
                    // fallback so a completed-partial 529 keeps its streamed output
                    // instead of re-fetching; a 529 that erred before any block
                    // completed (no output) falls through to the fallback as before.
                    Err(mut f)
                        if (crate::streaming_loop::partial_has_output(&f.partial)
                            || stream_decision
                                == Some(
                                    llm_runtime::model::stream_recovery::Decision::KeepPartial,
                                ))
                            && crate::streaming_loop::partial_finalize_cause(&f.error)
                                .is_some() =>
                    {
                        f.partial_close.close(orch.output.as_ref()).await;
                        let cause = crate::streaming_loop::partial_finalize_cause(&f.error)
                            .expect("finalize cause present (guarded above)");
                        let mut partial = f.partial;
                        // cc `gm=vd?"tool_use":"end_turn"`: a dispatched tool_use makes
                        // this a tool turn, else a natural end.
                        let synthesized_stop_reason = if partial.tool_uses.is_empty() {
                            "end_turn"
                        } else {
                            "tool_use"
                        };
                        partial.stop_reason = Some(synthesized_stop_reason.to_string());
                        cost_receipt = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &model,
                            model_profile.as_deref(),
                            &partial,
                            api_call_started.elapsed(),
                        );
                        // cc `_r.length`: one yielded message per completed content block.
                        let blocks_yielded =
                            partial.assistant_blocks.len() + partial.tool_uses.len();
                        if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
                            let mut md = telemetry::LogEventMetadata::new();
                            md.insert(
                                "model".into(),
                                telemetry::AnalyticsValue::String(model.clone()),
                            );
                            md.insert(
                                "blocks_yielded".into(),
                                telemetry::AnalyticsValue::Int(
                                    i64::try_from(blocks_yielded).unwrap_or(i64::MAX),
                                ),
                            );
                            md.insert(
                                "has_output".into(),
                                telemetry::AnalyticsValue::Bool(
                                    crate::streaming_loop::partial_has_output(&partial),
                                ),
                            );
                            md.insert(
                                "synthesized_stop_reason".into(),
                                telemetry::AnalyticsValue::String(
                                    synthesized_stop_reason.to_string(),
                                ),
                            );
                            md.insert(
                                "cause".into(),
                                telemetry::AnalyticsValue::String(cause.as_str().to_string()),
                            );
                            if let Some(rid) = orch.api.last_request_id() {
                                md.insert(
                                    "request_id".into(),
                                    telemetry::AnalyticsValue::String(rid),
                                );
                            }
                            bus.log_event("tengu_streaming_partial_finalized", md).await;
                        }
                        // Arm the notice + terminal-end sites below; the partial flows
                        // through the normal billing/persist/tool-drive path first.
                        partial_finalize = Some(cause);
                        (partial, None)
                    }
                    Err(mut f)
                        if exec.tools.is_empty()
                            && settlement.query_rows.is_empty()
                            && (stream_decision
                            == Some(
                                llm_runtime::model::stream_recovery::Decision::RetryWithoutStreaming,
                            )
                            || (matches!(
                                &f.error,
                                OrchestratorError::Streaming(error)
                                    if matches!(
                                        error.classification_source(),
                                        LlmError::Overloaded { .. } | LlmError::ProviderInternal
                                    )
                                ) && stream_decision.is_none()
                                && !is_env_truthy(
                                    std::env::var(branding::DISABLE_NONSTREAMING_FALLBACK_ENV)
                                        .as_deref()
                                        .ok(),
                                )
                                && retry_scope.take_non_streaming_fallback())) =>
                    {
                        f.partial_close.close(orch.output.as_ref()).await;
                        // The opened stream may already have written cache
                        // tokens at message_start. Record it before the
                        // replacement request overwrites its captured prefix.
                        if let Some(usage) = f.partial.usage.as_ref() {
                            orch.record_prompt_cache_usage(usage).await;
                        }
                        did_fall_back_to_non_streaming = true;
                        let failed_stream_outlasted_timeout = stream_failed_long
                            && matches!(
                                &f.error,
                                OrchestratorError::Streaming(error)
                                    if matches!(
                                        error.classification_source(),
                                        LlmError::Overloaded { .. }
                                            | LlmError::ProviderInternal
                                            | LlmError::ProviderTimeout { .. }
                                    )
                            );
                        let fallback_result = Self::fallback_after_stream_error(
                                orch,
                                f.error,
                                outgoing_history_rewriter.as_ref(),
                                deferred_reminder.as_ref(),
                                date_change_reminder.as_ref(),
                                &mut turn_reminders,
                                &mut guarded_async_hook_reminders,
                                &context_announcements,
                                &wire_tools,
                                available_tool_names.clone(),
                                skip_global_cache_for_system_prompt,
                                native_anthropic_request_context.clone(),
                                system_prompt,
                                user_cancel,
                                display_hook_active,
                                loop_state.native_query_depth,
                                assistant_id,
                                cost_scope.as_ref(),
                                api_call_started,
                                &retry_scope,
                                failed_stream_outlasted_timeout,
                            )
                            .await;
                        orch.record_native_afk_beta_rejection_if_any();
                        match fallback_result? {
                            StreamErrorFallback::Recovered {
                                pumped,
                                executor,
                                request_history,
                                cost_receipt: fallback_cost_receipt,
                            } => {
                                exec = executor;
                                cache_request_history = request_history;
                                cost_receipt = fallback_cost_receipt;
                                assistant_session_append_dispatched = true;
                                (pumped, None)
                            }
                            StreamErrorFallback::Declined(message_id) => {
                                return Ok(PumpStreamingOutcome::Complete(message_id));
                            }
                            StreamErrorFallback::Cancelled => {
                                orch.settle_stream_error_attempt(&mut exec, &mut f.partial, &mut settlement, assistant_id, INTERRUPT_MESSAGE).await;
                                let cost = orch.snapshot_cost_real().await;
                                orch.emit_turn_terminal("aborted_streaming", &cost).await;
                                return Ok(PumpStreamingOutcome::Complete(assistant_id));
                            }
                        }
                    }
                    // #10: RateLimited/Overloaded/RepeatedOverloaded keep dedicated
                    // downstream handling — propagate.
                    Err(mut f) if crate::turn_loop::is_carveout_propagated(&f.error) => {
                        f.partial_close.close(orch.output.as_ref()).await;
                        if let Some(usage) = f.partial.usage.as_ref() {
                            orch.record_prompt_cache_usage(usage).await;
                        }
                        let other = f.error;
                        orch.settle_stream_error_attempt(
                            &mut exec,
                            &mut f.partial,
                            &mut settlement,
                            assistant_id,
                            &other.to_string(),
                        )
                        .await;
                        crate::server_fallback::flush_pending_notice(orch).await;
                        return Err(other);
                    }
                    // #10: any other mid-stream model/runtime error (e.g. Transport)
                    // ends the turn GRACEFULLY as `model_error` (faithful port of the
                    // `query.ts` catch) rather than bubbling a hard error / phantom
                    // interrupt. Reached when the partial finalize above did NOT apply —
                    // either no recoverable output existed before the error (for
                    // example, an incomplete tool block) or the error is not a
                    // finalize class. The assistant message for a partial-with-real-
                    // -output turn is persisted by the finalize arm above; here nothing
                    // was persisted (TS `yieldMissingToolResultBlocks` no-op).
                    Err(mut f) => {
                        f.partial_close.close(orch.output.as_ref()).await;
                        if let Some(usage) = f.partial.usage.as_ref() {
                            orch.record_prompt_cache_usage(usage).await;
                        }
                        let other = f.error;
                        orch.settle_stream_error_attempt(
                            &mut exec,
                            &mut f.partial,
                            &mut settlement,
                            assistant_id,
                            &other.to_string(),
                        )
                        .await;
                        // Classify the typed mid-stream error (`Flp`/`KNn`) into the
                        // api-error envelope; the message text stays verbatim.
                        let env = classify_api_error(&other);
                        let provider_status = match &other {
                            OrchestratorError::ApiCall(inner) | OrchestratorError::Streaming(inner) => inner.http_status(),
                            _ => None,
                        };
                        let error_text =
                            crate::turn_loop::orchestrator_error_text_source(&other, &model);
                        orch.output.note_model_error_kind(crate::turn_loop::model_error_kind(&other));
                        let id =
                            crate::turn_loop::surface_model_error(orch, error_text, env, provider_status).await;
                        crate::server_fallback::flush_pending_notice(orch).await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.mark_mod_turn_error();
                        orch.emit_turn_terminal("model_error", &cost).await;
                        return Ok(PumpStreamingOutcome::Complete(id));
                    }
                }
            }
        };
        let assistant_id = pumped.replacement_message_id.unwrap_or(assistant_id);
        let request_model = model.clone();
        let request_profile = model_profile.clone();
        let mut response_model = request_model.clone();
        let mut response_profile = request_profile.clone();
        if let Some(served_model) = pumped.served_model.as_ref() {
            response_model.clone_from(served_model);
        }
        if let Some(info) = pumped
            .server_fallback_events
            .iter()
            .rev()
            .find(|info| matches!(info.event.reason.as_str(), "refusal" | "sticky"))
        {
            response_profile = Some(info.profile.clone());
        }
        if cost_receipt.is_none() {
            cost_receipt = Self::begin_stream_cost_response(
                orch,
                cost_scope.as_ref(),
                &request_model,
                request_profile.as_deref(),
                &pumped,
                api_call_started.elapsed(),
            );
        }
        let next_mod_response = if turn_step_hook_active {
            match remaining {
                Some(stream) => {
                    let exec = crate::streaming_executor::StreamingToolExecutor::try_new_for_query(
                        orch,
                        query_history.clone(),
                        loop_state.native_query_depth,
                        user_cancel.clone(),
                        available_tool_names.clone(),
                    )
                    .await?;
                    Some(OpenedStreamingIteration {
                        opened: OpenedModelStream::Stream(stream),
                        exec,
                        query_history: query_history.clone(),
                        retry_scope: retry_scope.clone(),
                        model: model.clone(),
                        model_profile: model_profile.clone(),
                        outgoing_history_rewriter,
                        turn_reminders,
                        tool_result_reminder_suffixes,
                        native_anthropic_request_context,
                        guarded_async_hook_reminders,
                        context_announcements,
                        wire_tools: wire_tools.clone(),
                        available_tool_names,
                        skip_global_cache_for_system_prompt,
                        deferred_reminder,
                        date_change_reminder,
                        assistant_id: MessageId::new(),
                        partial_finalize: None,
                        partial_finalize_notice_id: None,
                        turn_id: turn_id.clone(),
                        display_hook_active,
                        turn_step_hook_active,
                        mod_model_state,
                        api_call_started: std::time::Instant::now(),
                        stream_started,
                        api_success_message_count,
                        api_success_message_tokens,
                        did_fall_back_to_non_streaming: false,
                        cost_scope,
                        cost_receipt: None,
                    })
                }
                None => None,
            }
        } else {
            None
        };
        Ok(PumpStreamingOutcome::Pumped(PumpedStreamingIteration {
            pumped,
            settlement,
            next_mod_response,
            exec,
            request_model,
            model: response_model,
            model_profile: response_profile,
            wire_tools,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            api_call_started,
            api_success_message_count,
            api_success_message_tokens,
            did_fall_back_to_non_streaming,
            cache_request_history,
            cache_inline_context,
            assistant_session_append_dispatched,
            pre_batch_mcp_tool_count,
            cost_receipt,
        }))
    }

    async fn finalize_iteration<'a>(
        orch: &ConversationOrchestrator,
        pumped_iteration: PumpedStreamingIteration<'a>,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
    ) -> Result<FinalizedStreamingIteration<'a>, OrchestratorError> {
        let PumpedStreamingIteration {
            mut pumped,
            mut settlement,
            next_mod_response,
            mut exec,
            request_model,
            model,
            model_profile,
            wire_tools,
            assistant_id,
            partial_finalize,
            mut partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            api_call_started,
            api_success_message_count,
            api_success_message_tokens,
            did_fall_back_to_non_streaming,
            cache_request_history,
            cache_inline_context,
            assistant_session_append_dispatched,
            pre_batch_mcp_tool_count,
            cost_receipt,
        } = pumped_iteration;

        // Inline tool descriptions are committed only after a complete,
        // non-API-error response. Partial finalization is an error surface and
        // must not advance the persisted snapshot.
        if partial_finalize.is_none() {
            orch.record_inline_prompt_tools_after_success(&wire_tools)
                .await;
        }

        // BILLING: record streaming-turn usage into CostTracker — mirrors the
        // non-streaming path in `turn_loop.rs`. #5 (main-loop parity): pass
        // the REAL wall-clock duration (stream-open → pump completion) and
        // the REAL connect-phase retry count (`last_retry_count()`) instead
        // of the previous hardcoded `Duration::ZERO` / `0`.
        if let Some(ref usage) = pumped.usage {
            orch.record_prompt_cache_usage(usage).await;
            // #55: cache this response's total input tokens (the `Xtt`
            // last-usage snapshot) for the fixed-prefix overflow guard.
            orch.record_response_input_tokens(usage);
        }
        if let (Some(usage), Some(cost_receipt)) = (pumped.usage.as_ref(), cost_receipt) {
            let cache_read = usage.counts().cache_read_tokens;
            let cache_create = usage.counts().cache_write_tokens;
            let model_ref =
                crate::cost_wiring::model_ref_from_string(&model, model_profile.as_deref());
            let elapsed = api_call_started.elapsed();
            let retries = orch.streaming_api.last_retry_count();
            let settlement = cost_receipt.settle().await;
            let cost_for_this_call = settlement.observed_nano_usd();
            orch.model_runtime
                .api_calls_recorded
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Err(error) = settlement.persistence_result() {
                orch.note_cost_settlement_failure(error).await;
            }
            // strict-parity (2.1.195): fire `tengu_api_success` on the
            // streaming per-request success path (claude
            // `j("tengu_api_success", {...})`). `tengu_cost_recorded`
            // was port-only and dropped.
            //
            // P1-04: a partial-stream finalize is NOT a per-request success —
            // cc records cost (`Ae+=zhe`, kept above) but does NOT emit
            // `tengu_api_success` (it already fired `tengu_streaming_partial_finalized`).
            // Skip the success emit when this turn was finalized from a partial.
            if let Some(bus) = orch
                .model_runtime
                .analytics_bus
                .as_ref()
                .filter(|_| partial_finalize.is_none())
            {
                #[allow(clippy::cast_possible_truncation)]
                let dur_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                cost::emit_api_success(
                    bus,
                    &cost::ApiSuccessFields {
                        model: model.clone(),
                        input_tokens: usage.counts().input_tokens,
                        output_tokens: usage
                            .counts()
                            .output_tokens
                            .saturating_sub(usage.counts().reasoning_tokens),
                        cached_input_tokens: cache_read,
                        uncached_input_tokens: cache_create,
                        duration_ms: dur_ms,
                        duration_ms_including_retries: dur_ms,
                        attempt: retries + 1,
                        cost_nano_usd: cost_for_this_call,
                        provider: crate::cost_wiring::provider_tag(&model_ref.provider),
                        stop_reason: pumped.stop_reason.clone(),
                        request_id: orch.api.last_request_id(),
                        message_count: api_success_message_count,
                        message_tokens: api_success_message_tokens,
                        did_fall_back_to_non_streaming,
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
                        fast_mode: usage.inference.service_tier
                            == Some(llm_runtime::services::sdk::protocol::ServiceTier::Fast),
                        time_since_last_api_call_ms: orch.record_api_call_gap_ms(),
                    },
                )
                .await;
            }
        }

        // In-Loop Compaction Batch 6: snapshot the cache-safe prompt prefix
        // after a successful stream (streaming twin of the batched save).
        // `session.history` here equals the streamed snapshot — the streaming
        // path does not mutate history mid-call — taken before the assistant
        // reply is appended below. Strict no-op when no slot is wired.
        if partial_finalize.is_none() {
            orch.save_cache_safe_params_for_request(
                system_prompt.as_ref(),
                &model,
                &wire_tools,
                &cache_request_history,
                cache_inline_context.as_ref(),
            )
            .await?;
        }

        // Task 8 (llm-runtime future-work batch 3): the streamed call (or
        // its non-streaming 529 fallback) completed — forward the
        // adapter's unified rate-limit snapshot when it changed since the
        // last emission. `orch.api` is the same `ProviderApiAdapter` as
        // `orch.streaming_api` in production; the adapter records headers
        // on the `drive_stream` connect-success path too.
        orch.emit_rate_limit_if_changed().await;
        // Task 2 (batch 5): same seam, raw per-window snapshot.
        orch.emit_raw_utilization_if_changed().await;

        // 4. Assemble + append the assistant message.
        // `assistant_id` was pre-allocated before the stream (mid-stream
        // dispatch hands it to the executor as each tool registers). An
        // accepted server-fallback display replacement resets this same live
        // assistant row and retains its identity, so this is also the ID sent
        // by the final `MessageIdentity` and persisted below.
        let assistant_msg = pumped_assistant_message(&pumped, assistant_id);
        orch.note_turn_model_stop_reason(pumped.stop_reason.as_deref());
        let publication_fence = exec.publication_fence();
        let publication_guard = orch
            .prompt_runtime
            .guarded_prompt_message_guard(assistant_id)
            .await
            .unwrap_or_else(|| Arc::new(publication_fence.clone()));
        if !orch
            .append_streamed_assistant_to_history(
                &assistant_msg,
                Some(publication_guard),
                !pumped.assistant_rows.is_empty(),
            )
            .await
        {
            return Ok(FinalizedStreamingIteration::Complete(assistant_id));
        }
        orch.record_main_loop_assistant_usage(
            &assistant_msg,
            pumped.served_model.as_deref().unwrap_or(&model),
            pumped.usage.as_ref(),
        )
        .await;
        // The first-frame metric is latched at event receipt. Publish TTFT
        // only once the assistant row passes the conversation's guard.
        pumped.commit_accepted_assistant_timing();
        orch.record_mod_turn_response(
            &assistant_msg,
            pumped.usage.as_ref(),
            &model,
            pumped.stop_reason.as_deref(),
            pumped.stop_details.as_ref(),
        );

        // P2-04 (MessageDisplay `displayContent`): completed-message pass.
        // Once the assistant text blocks are finalized, fire `MessageDisplay`
        // with `final:true` + the joined visible text and render the result
        // ON SCREEN — substituting the hook's `displayContent` when present,
        // else the original text. The stored `assistant_msg` / JSONL keep the
        // ORIGINAL content (claude-code stores the override on a separate
        // `displayedMessageContent`, never `message.content`). Gated on a
        // registered `MessageDisplay` hook — when active the live per-token
        // deltas were suppressed in the pump, so this is the single on-screen
        // render; when inactive this whole block is skipped (byte-identical).
        if display_hook_active {
            let joined_projection = joined_visible_text_projection(&pumped.assistant_blocks);
            let joined = joined_projection.string_at("").unwrap_or_default();
            // claude-code `Qff`: skip firing entirely when the joined text is
            // empty (`if(s==="")return i`).
            if !joined.is_empty() {
                let on_screen = orch
                    .fire_message_display_completed(&turn_id, &joined_projection)
                    .await
                    .map(hooks::ExactHookText::into_projection)
                    .unwrap_or(joined_projection);
                orch.output.emit_text(&on_screen).await;
            }
        }

        // Finding #73 (streaming twin): advance the per-turn todo/task
        // reminder counters for THIS assistant turn, then reset
        // `turns_since_last_todo_write` if this turn invoked the variant's
        // "recent use" tool (TodoWrite / TaskCreate / TaskUpdate). Bump THEN
        // reset so a TodoWrite turn lands at 0 (matching the binary scan that
        // excludes the TodoWrite message itself). Mirrors the batched twin.
        orch.bump_reminder_turn_counters().await;
        let invoked_tool_names: Vec<String> =
            pumped.tool_uses.iter().map(|t| t.name.clone()).collect();
        orch.note_todo_reminder_tool_call(&invoked_tool_names).await;

        // DEFERRED-3: advance the interrupt-guard's reported final id to this
        // turn's assistant message.
        loop_state.last_message_id = assistant_id;
        // WRITE-side per-block split (claude.ts:2171-2211): persist the turn
        // as one single-block assistant JSONL line per content block, sharing
        // the turn's inner `message.id` with distinct top-level uuids, and
        // capture each `tool_use`'s line uuid so its `tool_result` parents to
        // ITS line (TS `sourceToolAssistantUUID`) — NOT one shared per-turn
        // parent. The in-memory `s.history` above stays the single merged
        // assistant message (the Anthropic request needs all blocks in one
        // assistant turn).
        // Raw Anthropic `usage` object for the persisted BetaMessage envelope
        // (the streaming codec retains it on `Usage::provider_metadata`).
        let assistant_usage = pumped.usage.as_ref().map(assistant_usage_value);
        // The Anthropic `request-id` response header for THIS turn → the
        // top-level `requestId` on each persisted assistant line. The
        // adapter records it from the stream connect-success / non-stream
        // headers (the same `record_rate_limit_from_headers` pass), so it is
        // the just-completed call's id here. `orch.api` is the same adapter
        // as `orch.streaming_api` in production; the fallback non-stream call
        // records it on `orch.api` too.
        let request_id = orch.api.last_request_id();
        // stream-json P1: signal the message boundary to the output sink so
        // `StreamJsonStream` can flush its accumulated assistant frame.
        orch.output
            .emit_assistant_message_identity(&assistant_id)
            .await;
        orch.output
            .emit_message_boundary(pumped.stop_reason.as_deref(), request_id.as_deref())
            .await;
        let mut tool_use_parent_uuids = pumped.assistant_tool_parent_uuids.clone();
        if pumped.assistant_rows.is_empty() {
            tool_use_parent_uuids = if assistant_session_append_dispatched {
                orch.persist_preappended_assistant_per_block(
                    &assistant_msg,
                    assistant_usage.as_ref(),
                    request_id.as_deref(),
                    publication_fence.clone(),
                )
                .await
            } else {
                orch.persist_assistant_per_block(
                    &assistant_msg,
                    assistant_usage.as_ref(),
                    request_id.as_deref(),
                    Some(publication_fence.clone()),
                )
                .await
            };
        } else {
            for row in &mut pumped.assistant_rows {
                row.stop_reason.clone_from(&pumped.stop_reason);
                row.stop_details.clone_from(&pumped.stop_details);
                row.usage.clone_from(&pumped.usage);
            }
            // The stream-local journal retains interleaved assistant/tool rows
            // until terminal fields are available, then writes only its new
            // suffix in source event order. Raw K rows still become the single
            // merged assistant model message above.
            orch.flush_stream_event_journal(&mut settlement, &mut pumped.assistant_rows)
                .await;
            tool_use_parent_uuids.extend(settlement.tool_use_parent_uuids.clone());
        }
        crate::server_fallback::flush_pending_notice(orch).await;
        // Fallback parent (the LAST persisted block's uuid) for any
        // tool_result whose tool_use id is missing from the map (defensive).
        let assistant_uuid = orch.transcript.last_jsonl_uuid.lock().await.clone();

        // P1-04 (cc 2.1.199): after the finalized partial assistant is
        // persisted, yield the byte-exact incomplete-response notice as its own
        // api-error assistant message — cc yields `tu({content:…, error:"server_error"})`
        // RIGHT AFTER the patched partial and BEFORE any tool_results run. The
        // notice's api-error category is hardcoded `server_error` regardless of
        // the underlying finalize cause (cc `error:"server_error"`). Persisted
        // without the `tengu_query_error` telemetry (that fires only from the
        // top-level `model_error` catch, not this finalize path).
        if let Some(cause) = partial_finalize {
            let env = ApiErrorEnvelope {
                error: Some("server_error"),
                api_error_status: None,
                terminal_reason: None,
                inner_stop_reason: None,
                inner_stop_details: None,
                refused_message_id: None,
                truncated_after_output: crate::streaming_loop::partial_is_text_continuable(&pumped),
            };
            partial_finalize_notice_id = Some(
                crate::turn_loop::surface_api_error_notice(
                    orch,
                    cause
                        .notice(crate::streaming_loop::partial_has_output(&pumped))
                        .into(),
                    env,
                )
                .await,
            );
        }

        // #5 aborted_streaming vs aborted_tools disambiguation (faithful port
        // of claude-code's TWO distinct abort checkpoints): query.ts:1015 runs
        // RIGHT AFTER `callModel`, BEFORE the tool-completion drive — an abort
        // observed there is `aborted_streaming` + `createUserInterruptionMessage({toolUse:false})`
        // (`[Request interrupted by user]`). query.ts:1485 runs AFTER the tool
        // drive — an abort observed only there is `aborted_tools` +
        // `createUserInterruptionMessage({toolUse:true})`
        // (`[Request interrupted by user for tool use]`). Capture the
        // checkpoint-1015 state HERE (before the drive); the single post-drive
        // abort check below picks the reason/message from it. The common
        // "interrupt during thinking/streaming" case (incl. no-tool responses)
        // lands as `aborted_streaming`, not the previous mislabel
        // `aborted_tools`. `None` token (plain `run_turn_streaming`) → always
        // false → byte-identical to before. The drive loop below still flushes
        // synthetic REJECT_MESSAGE tool_results (the executor was built with the
        // cancel token, so pending/queued tools reject rather than execute) —
        // matching ref's `getRemainingResults()` at the 1015 path.
        let aborted_during_stream = user_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);

        // Ko gates both refusal tool settlement and the same-model retry. When
        // Ko is false, Native keeps the ordinary tool executor live: queued
        // calls can finish, their results feed the generic needsFollowUp pass,
        // and a refusal API-error row is persisted before those results.
        let native_catalog_refusal = pumped.stop_reason.as_deref() == Some("refusal")
            && pumped.server_fallback_events.is_empty()
            && !aborted_during_stream
            && crate::turn_loop::native_refusal_retry_route(orch, &request_model).await;
        let refusal_retry_enabled = crate::turn_loop::native_refusal_retry_enabled(
            &orch.config.query_source,
            loop_state.recovery.refusal_retry_spent,
        );
        let refusal_tool_uses_settled = native_catalog_refusal && refusal_retry_enabled;
        if native_catalog_refusal && !refusal_retry_enabled && !pumped.tool_uses.is_empty() {
            pumped.refusal_api_error_staged = orch
                .stage_refusal_api_error_row(
                    &request_model,
                    pumped.stop_details.as_ref(),
                    pumped.response_message_id.as_deref(),
                )
                .await?;
        }

        let (tool_prevent_continuation, post_tool_batch_dispatch) = orch
            .drive_streaming_tools(
                &mut exec,
                &mut pumped,
                &mut settlement,
                &tool_use_parent_uuids,
                assistant_id,
                &assistant_uuid,
                refusal_tool_uses_settled,
            )
            .await?;

        Ok(FinalizedStreamingIteration::Finalized(
            FinalizedStreamingData {
                pumped,
                next_mod_response,
                request_model,
                native_catalog_refusal,
                assistant_id,
                tool_prevent_continuation,
                post_tool_batch_dispatch,
                pre_batch_mcp_tool_count,
                partial_finalize,
                partial_finalize_notice_id,
                aborted_during_stream,
                refusal_tool_uses_settled,
            },
        ))
    }

    async fn next_mod_response<'a>(
        next: Option<OpenedStreamingIteration<'a>>,
    ) -> Option<OpenedStreamingIteration<'a>> {
        let mut opened = next?;
        let OpenedModelStream::Stream(mut tail) = opened.opened else {
            unreachable!("a Mod continuation always owns a stream")
        };
        let first = tail.next().await?;
        if let Some((model, profile)) = opened
            .mod_model_state
            .as_ref()
            .and_then(|state| state.lock().unwrap().clone())
        {
            opened.model = model;
            opened.model_profile = profile;
        }
        if let Ok(HistoryEvent::MessageStart { response }) = &first {
            opened.model = response.model.clone();
        }
        opened.api_call_started = std::time::Instant::now();
        opened.opened = OpenedModelStream::Stream(
            futures::stream::once(async move { first })
                .chain(tail)
                .boxed(),
        );
        Some(opened)
    }

    /// One iteration of the streaming turn loop: prepare → pump → finalize →
    /// abort checkpoint → disposition.
    ///
    /// Everything the loop does AFTER its top-of-loop guards lives here, so the
    /// loop itself is the guard sequence plus a three-way branch on what this
    /// returned — the shape §5.4 wants both turn loops to share. The guards stay
    /// outside on purpose: §3.5 gives each entry a different order, and the
    /// streaming cancel guard runs between them and this call.
    async fn run_round(
        orch: &ConversationOrchestrator,
        system_prompt: &Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
        in_human_turn: bool,
        ack_failure: &mut bool,
    ) -> Result<StepExit, OrchestratorError> {
        let mut step_cache = None;
        loop {
            let selected = {
                let session = orch.session.lock().await;
                crate::query_model::ModelRoute {
                    model: session.model.clone(),
                    profile: session.model_profile.clone(),
                }
            };
            let selected = crate::scheduled_turn::current().map_or(selected, |settings| {
                crate::query_model::ModelRoute {
                    model: settings.model,
                    profile: Some(settings.provider),
                }
            });
            // A user's model switch during the query supersedes its fallback.
            if loop_state.selected_route.as_ref() != Some(&selected) {
                loop_state.selected_route = Some(selected);
                loop_state.serving_route = None;
                loop_state.fallback_index = 0;
            }
            let route = loop_state.serving_route.clone();
            let context = crate::query_model::fallback_target_context(
                orch,
                &loop_state.selected_route.as_ref().unwrap().model,
                route.as_ref().map(|route| route.model.as_str()),
            )
            .await;
            // This iteration future contains the prepare/pump/finalize state
            // machine. Keep it off `run_round`'s poll stack while the two
            // task-local scopes wrap the same future.
            let iteration = Self::run_round_for_model(
                orch,
                system_prompt,
                user_cancel,
                loop_state,
                in_human_turn,
                &mut step_cache,
            );
            let outcome = crate::query_model::ROUTE
                .scope(
                    route,
                    lingxi_core::host::refusal_driver::scope_fallback_target(context, iteration),
                )
                .await?;
            if let Some(exit) = outcome {
                return Ok(exit);
            }
            if token_aborted(user_cancel) {
                // Rejoin the outer cancellation checkpoint before any dispatch.
                return Ok(StepExit::Continue);
            }
        }
    }

    async fn advance_model(
        orch: &ConversationOrchestrator,
        loop_state: &mut TurnLoopState,
        failed: &str,
        profile: Option<&str>,
        reason: &str,
    ) -> Result<(), OrchestratorError> {
        let primary = loop_state
            .selected_route
            .as_ref()
            .map_or(failed, |route| route.model.as_str());
        let chain = crate::query_model::configured_chain(
            orch.config.fallback_model.as_deref().unwrap_or_default(),
            primary,
        );
        let (index, target) =
            crate::query_model::advance(&chain, loop_state.fallback_index, failed, reason);
        let target = target.ok_or_else(|| {
            OrchestratorError::Internal("configured model fallback chain exhausted".into())
        })?;
        let current = crate::query_model::ModelRoute {
            model: failed.into(),
            profile: profile.map(str::to_owned),
        };
        let listings = orch.api.list_model_listings();
        let next = crate::query_model::resolve(target, &current, &listings);
        loop_state.fallback_index = index.saturating_add(1);
        loop_state.serving_route = Some(next.clone());
        // Native Dt continues in place before creating a model-change notice.
        if index >= chain.len() {
            return Ok(());
        }
        let facts = |route: &crate::query_model::ModelRoute| {
            let listing = listings.iter().find(|listing| {
                listing.request_model == route.model
                    && route
                        .profile
                        .as_ref()
                        .is_none_or(|profile| listing.provider_id == *profile)
            });
            let label = listing.map_or_else(
                || route.model.clone(),
                |listing| listing.display_model.clone(),
            );
            let window = orch
                .api
                .context_window_tokens(&route.model, route.profile.as_deref());
            (label, window)
        };
        let (from_label, from_window) = facts(&current);
        let (to_label, to_window) = facts(&next);
        let content =
            crate::query_model::server_error_notice(&from_label, &to_label, from_window, to_window);
        let content_projection = Utf16JsonProjection::from(content.as_str());
        let id = MessageId::new();
        let metadata = lingxi_core::types::ModelFallbackMetadata {
            trigger: reason.into(),
            original_model: failed.into(),
            fallback_model: next.model,
        };
        let notice = ConversationMessage::System {
            api_system: None,
            id,
            content: content_projection.clone(),
            subtype: Some("model_fallback".into()),
            compact_metadata: None,
            refusal_fallback: None,
            model_fallback: Some(metadata.clone()),
        };
        let session_id = {
            let mut session = orch.session.lock().await;
            session.history.push(notice.clone());
            session.session_id
        };
        orch.persist_message_to_jsonl(&notice).await;
        orch.output
            .emit_model_fallback(&id, &session_id, &content_projection, &metadata)
            .await;
        Ok(())
    }

    fn run_round_for_model<'a>(
        orch: &'a ConversationOrchestrator,
        system_prompt: &'a Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &'a Option<CancellationToken>,
        loop_state: &'a mut TurnLoopState,
        in_human_turn: bool,
        step_cache: &'a mut Option<prepare::PreparedTurnStep>,
    ) -> BoxFuture<'a, Result<Option<StepExit>, OrchestratorError>> {
        Box::pin(async move {
            if crate::native_computer::uses_buffered_protocol(orch).await {
                Self::run_round_for_model_buffered(orch, system_prompt, user_cancel, loop_state)
                    .await
            } else {
                Self::run_round_for_model_streaming(
                    orch,
                    system_prompt,
                    user_cancel,
                    loop_state,
                    in_human_turn,
                    step_cache,
                )
                .await
            }
        })
    }

    #[inline(never)]
    fn run_round_for_model_buffered<'a>(
        orch: &'a ConversationOrchestrator,
        system_prompt: &'a Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &'a Option<CancellationToken>,
        loop_state: &'a mut TurnLoopState,
    ) -> BoxFuture<'a, Result<Option<StepExit>, OrchestratorError>> {
        Box::pin(async move {
            let future = crate::native_computer::scope_buffered_stream(
                crate::turn_loop::execute_one_turn_with_recovery_tracked(
                    orch,
                    system_prompt.as_ref(),
                    &mut loop_state.recovery,
                    Some((
                        &loop_state.mod_turn_id,
                        loop_state.turn_count.saturating_sub(1),
                    )),
                    loop_state.native_query_depth,
                ),
            );
            let result = match user_cancel {
                Some(cancel) => {
                    crate::native_computer::scope_turn_cancel(cancel.clone(), future).await
                }
                None => future.await,
            };
            if token_aborted(user_cancel) {
                crate::native_computer::cleanup(orch).await?;
                return Ok(Some(StepExit::ReturnDirect(ConversationOutcome::EndTurn {
                    turn_count: loop_state.turn_count,
                    final_message_id: MessageId::new(),
                })));
            }
            let (step, tokens) = result?;
            return match orch.run_batched_round(loop_state, step, tokens).await {
                super::TurnEndVerdict::Continue => Ok(Some(StepExit::Continue)),
                super::TurnEndVerdict::EndTurn(message_id) => {
                    Ok(Some(StepExit::FinishThroughEpilogue(message_id)))
                }
                super::TurnEndVerdict::StopHookTerminated(outcome) => {
                    Ok(Some(StepExit::ReturnDirect(outcome)))
                }
                super::TurnEndVerdict::MaxTurns => Err(OrchestratorError::MaxTurnsReached {
                    max_turns: orch.config.max_turns,
                }),
            };
        })
    }

    #[inline(never)]
    fn run_round_for_model_streaming<'a>(
        orch: &'a ConversationOrchestrator,
        system_prompt: &'a Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        user_cancel: &'a Option<CancellationToken>,
        loop_state: &'a mut TurnLoopState,
        in_human_turn: bool,
        step_cache: &'a mut Option<prepare::PreparedTurnStep>,
    ) -> BoxFuture<'a, Result<Option<StepExit>, OrchestratorError>> {
        Box::pin(async move {
            let mut continuation: Option<OpenedStreamingIteration<'_>> = None;
            loop {
                let pumped = if let Some(opened) = continuation.take() {
                    let retry_scope = opened.retry_scope.clone();
                    orch.set_tool_frame_buffering(true).await;
                    Self::pump_opened_iteration(
                        orch,
                        opened,
                        system_prompt,
                        user_cancel,
                        loop_state,
                        retry_scope,
                    )
                    .await
                } else {
                    let prepared = match Self::prepare_iteration(
                        orch,
                        system_prompt,
                        user_cancel,
                        loop_state,
                        in_human_turn,
                        step_cache.as_ref(),
                    )
                    .await?
                    {
                        PrepareStreamingOutcome::Ready(iteration) => iteration,
                        PrepareStreamingOutcome::Complete(message_id) => {
                            return Ok(Some(StepExit::FinishThroughEpilogue(message_id)));
                        }
                    };
                    if orch.config.fallback_model.is_some() {
                        *step_cache = Some(prepared.step.clone());
                    }
                    Self::pump_iteration(orch, prepared, system_prompt, user_cancel, loop_state)
                        .await
                };
                let pumped_iteration = match pumped {
                    Err(error) => {
                        // `open_iteration` enables SDK frame buffering before the
                        // stream is opened. Every error path must release that
                        // session-scoped buffer or later tool frames disappear.
                        orch.set_tool_frame_buffering(false).await;
                        return Err(error);
                    }
                    Ok(PumpStreamingOutcome::Pumped(iteration)) => iteration,
                    Ok(PumpStreamingOutcome::Complete(message_id)) => {
                        orch.set_tool_frame_buffering(false).await;
                        return Ok(Some(StepExit::FinishThroughEpilogue(message_id)));
                    }
                    Ok(PumpStreamingOutcome::ModelFallback) => {
                        orch.set_tool_frame_buffering(false).await;
                        return Ok(None);
                    }
                    Ok(PumpStreamingOutcome::Interrupted) => {
                        orch.set_tool_frame_buffering(false).await;
                        // Rejoin the top-of-loop cancellation checkpoint, which
                        // ends the turn as `aborted_streaming`.
                        return Ok(Some(StepExit::Continue));
                    }
                };

                let finalized = match Self::finalize_iteration(
                    orch,
                    pumped_iteration,
                    system_prompt,
                    user_cancel,
                    loop_state,
                )
                .await
                {
                    Ok(finalized) => finalized,
                    Err(error) => {
                        orch.set_tool_frame_buffering(false).await;
                        return Err(error);
                    }
                };
                let finalized = match finalized {
                    FinalizedStreamingIteration::Complete(message_id) => {
                        orch.set_tool_frame_buffering(false).await;
                        return Ok(Some(StepExit::FinishThroughEpilogue(message_id)));
                    }
                    FinalizedStreamingIteration::Finalized(finalized) => finalized,
                };
                let FinalizedStreamingData {
                    pumped,
                    next_mod_response,
                    request_model,
                    native_catalog_refusal,
                    assistant_id,
                    tool_prevent_continuation,
                    post_tool_batch_dispatch,
                    pre_batch_mcp_tool_count,
                    partial_finalize,
                    partial_finalize_notice_id,
                    aborted_during_stream,
                    refusal_tool_uses_settled,
                } = finalized;

                // A3: accumulate this step's output tokens (TS
                // `getTurnOutputTokens()`), ONCE, after the model step has returned
                // and before the disposition reads the running total.
                //
                // §3.4 wants a single accumulation boundary per step. This used to
                // sit inside `finalize_iteration`, where the two batched entries
                // accumulate in their loop bodies instead — three sites, one of them
                // a step deeper than the others. Moving it here puts all three on
                // the same boundary. Safe because nothing reads `global_turn_tokens`
                // between the old site and the disposition that consumes it, and
                // `streaming_budget_on_continues_then_stops_at_threshold` fails if
                // the accumulation lands after the budget check rather than before.
                loop_state.global_turn_tokens = loop_state
                    .global_turn_tokens
                    .saturating_add(pumped.output_tokens);

                // DEFERRED-3 / esc-interrupt FIX: "we were aborted" — the single
                // post-drive abort checkpoint. Once the user-interrupt token has fired,
                // the executor above already drained the bare REJECT_MESSAGE
                // `tool_result`s into history (model-visible). The turn MUST now STOP —
                // claude-code returns with NO further `callModel`, honoring
                // REJECT_MESSAGE's "STOP what you are doing and wait for the user".
                // Looping into the `Some("tool_use") => continue` arm below would (1)
                // issue a wasted extra round-trip after every ESC and (2) let a
                // Block-behavior tool emitted on that continuation actually EXECUTE
                // (`abort_reason_for` returns `None` for Block tools) despite the
                // interrupt — both of which claude-code structurally prevents by
                // returning here first. `None` token (plain `run_turn_streaming`) →
                // never fires → identical to before.
                //
                // #5: the terminal reason + interrupt message depend on WHICH ref
                // checkpoint observed the abort (captured in `aborted_during_stream`
                // before the drive): an abort already set when the stream ended is
                // `aborted_streaming` / `[Request interrupted by user]` (query.ts:1015,
                // `toolUse:false`); an abort that fired only DURING the tool drive is
                // `aborted_tools` / `[Request interrupted by user for tool use]`
                // (query.ts:1485, `toolUse:true`).
                if user_cancel
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                {
                    // Abort wins over a result-level end request. Results that were
                    // allowed to finish (notably Block-behavior tools) may have
                    // recorded one before this checkpoint, so drain the side table
                    // even though the end-turn path below is intentionally skipped.
                    let _ = orch
                        .take_pending_tool_result_turn_ends(
                            &pumped
                                .tool_uses
                                .iter()
                                .map(|tool_use| tool_use.id.clone())
                                .collect::<Vec<_>>(),
                        )
                        .await;
                    let cost = orch.snapshot_cost_real().await;
                    let (abort_reason, interruption_kind) = if aborted_during_stream {
                        ("aborted_streaming", QueryUserInterruptionKind::Streaming)
                    } else {
                        ("aborted_tools", QueryUserInterruptionKind::Tools)
                    };
                    orch.emit_turn_terminal(abort_reason, &cost).await;
                    // The persistence boundary filters non-user cancellations.
                    orch.inject_query_user_interruption(interruption_kind).await;
                    return Ok(Some(StepExit::FinishThroughEpilogue(assistant_id)));
                }

                // P1-04 (cc 2.1.199): a finalized partial ends the turn once its
                // dispatched tools have drained (above) and the incomplete-response
                // notice has been surfaced — cc `break e`s out of the stream loop after
                // yielding the notice; it does NOT re-enter the continuation logic. We
                // terminate here (reason `model_error`, matching the api-error catch)
                // rather than looping on the synthesized `tool_use`/`end_turn`, so the
                // user sees the partial + notice and can retry. The synthesized
                // stop_reason still rides on the persisted partial assistant line
                // (patched above) for resume fidelity.
                if partial_finalize.is_some() {
                    if crate::streaming_loop::partial_is_text_continuable(&pumped)
                        && crate::turn_loop::truncated_response_recovery_eligible(
                            &orch.config.query_source,
                            orch.prompt_is_interactive(),
                        )
                        && loop_state.recovery.max_output_tokens_recovery_count
                            < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT
                    {
                        let is_subagent = crate::turn_loop::truncated_response_recovery_is_subagent(
                            &orch.config.query_source,
                        );
                        let nudge = if is_subagent {
                            crate::turn_loop::TRUNCATED_RESPONSE_RECOVERY_NUDGE_SUBAGENT
                        } else {
                            crate::turn_loop::TRUNCATED_RESPONSE_RECOVERY_NUDGE_MAIN
                        };
                        orch.inject_meta_user_message(nudge).await;
                        loop_state.recovery.max_output_tokens_recovery_count = loop_state
                            .recovery
                            .max_output_tokens_recovery_count
                            .saturating_add(1);
                        loop_state.recovery.max_output_tokens_override = None;
                        loop_state.recovery.output_recovery_actions += 1;
                        return Ok(Some(StepExit::Continue));
                    }
                    // A finalized partial is terminal before normal tool-result
                    // disposition. Do not leave an end marker from a completed
                    // result live in the session-scoped side table.
                    let _ = orch
                        .take_pending_tool_result_turn_ends(
                            &pumped
                                .tool_uses
                                .iter()
                                .map(|tool_use| tool_use.id.clone())
                                .collect::<Vec<_>>(),
                        )
                        .await;
                    let cost = orch.snapshot_cost_real().await;
                    orch.mark_mod_turn_error();
                    orch.emit_turn_terminal("model_error", &cost).await;
                    return Ok(Some(StepExit::FinishThroughEpilogue(
                        partial_finalize_notice_id.unwrap_or(assistant_id),
                    )));
                }

                // A single Mod dispatch can yield several assistant responses. Wait
                // until this response's tools are drained, then pull the next frame
                // from the same worker stream before deciding a natural turn end.
                let next_mod_response = if pumped.tool_uses.is_empty() {
                    Self::next_mod_response(next_mod_response).await
                } else {
                    next_mod_response
                };
                if next_mod_response.is_some()
                    && pumped.tool_uses.is_empty()
                    && !matches!(
                        pumped.stop_reason.as_deref(),
                        Some("model_context_window_exceeded" | "refusal")
                    )
                {
                    orch.turn_span.note_assistant_response(0);
                    if pumped.stop_reason.as_deref() != Some("tool_use") {
                        loop_state.malformed_tool_use_retried = false;
                    }
                    continuation = next_mod_response;
                    continue;
                }

                return match orch
                    .decide_streaming_disposition(
                        loop_state,
                        &pumped,
                        &request_model,
                        native_catalog_refusal,
                        assistant_id,
                        tool_prevent_continuation,
                        post_tool_batch_dispatch,
                        pre_batch_mcp_tool_count,
                        refusal_tool_uses_settled,
                        ack_failure,
                    )
                    .await?
                {
                    StreamingIterationDisposition::Continue(transition) => {
                        if transition == crate::turn_loop::NativeQueryTransition::RefusalReissue {
                            loop_state.refusal_reissue_pending = true;
                        }
                        loop_state.native_query_depth = crate::turn_loop::native_query_depth_after(
                            loop_state.native_query_depth,
                            transition,
                        );
                        let next = if transition
                            == crate::turn_loop::NativeQueryTransition::RefusalReissue
                        {
                            // A refusal retry/fallback is a new model reissue,
                            // not the next pre-opened response from an old Mod
                            // stream. Returning to the outer loop runs the
                            // one-shot refusal-reissue guard before that call.
                            None
                        } else if pumped.tool_uses.is_empty() {
                            next_mod_response
                        } else {
                            Self::next_mod_response(next_mod_response).await
                        };
                        if let Some(mut opened) = next {
                            opened
                                .exec
                                .set_native_query_depth(loop_state.native_query_depth);
                            continuation = Some(opened);
                            continue;
                        }
                        Ok(Some(StepExit::Continue))
                    }
                    StreamingIterationDisposition::NaturalComplete(message_id) => {
                        // Pending input can arrive while the final response is
                        // streaming, after the top-of-loop drain has already run.
                        // Give it one final drain before committing the natural
                        // end-turn so it continues this same turn.
                        let injected = orch
                            .drain_mid_turn_input_until_cancelled(user_cancel, ack_failure)
                            .await?;
                        if injected && !token_aborted(user_cancel) {
                            return Ok(Some(StepExit::ContinueAfterFinalDrain));
                        }
                        // Publish only after the epilogue has settled and the
                        // final cancellation and budget checks have run.
                        Ok(Some(StepExit::FinishNaturalThroughEpilogue(message_id)))
                    }
                    StreamingIterationDisposition::ForcedComplete(message_id, terminal) => Ok(
                        Some(StepExit::FinishForcedThroughEpilogue(message_id, terminal)),
                    ),
                    // §3.6: the ONE disposition that skips the epilogue.
                    StreamingIterationDisposition::Return(outcome) => {
                        Ok(Some(StepExit::ReturnDirect(outcome)))
                    }
                };
            }
        })
    }

    pub(super) async fn run(self) -> Result<ConversationOutcome, OrchestratorError> {
        let Some(admission) = self.orch.capture_tool_dispatch_recovery_guard() else {
            return self.run_inner().await;
        };
        let guard: hooks::mods::ModTurnRecoveryGuard = Arc::new(move || {
            admission().map(|leases| leases.map(|lease| (lease.execution, lease.turn_work)))
        });
        hooks::mods::with_mod_turn_recovery_guard(guard, self.run_inner())
            .await
            .map_err(|error| match error {
                hooks::RecoveryAdmissionError::Interrupted => {
                    OrchestratorError::RecoveryInterrupted
                }
                hooks::RecoveryAdmissionError::Failure(error) => OrchestratorError::Internal(error),
            })?
    }

    async fn run_inner(self) -> Result<ConversationOutcome, OrchestratorError> {
        let Self {
            orch,
            prompt,
            mut images,
            user_cancel,
            message_id,
            transient_rewake,
            in_human_turn,
            queued_inputs,
            row_token,
            mut projected_content,
        } = self;

        // Startup Responses WebSocket prewarm is strictly opportunistic. A real
        // user turn must never wait for an in-flight `generate=false` request to
        // finish before it can open its own stream.
        orch.abort_startup_responses_websocket_prewarm();

        // 0. Build the system prompt for THIS turn.
        // claude-code `nre` precedence: `--system-prompt` (override) wins; else
        // the `--agent`-adopted main-thread agent's prompt; else the default.
        let system_prompt: Option<
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        > = Some(orch.provider_system_prompt().await?);
        ensure_mod_recovery_accepted()?;

        // A side query left unfinished when the previous user turn ended was
        // keyed to that previous prompt. Never surface it against new intent.
        orch.discard_stale_prefetches().await;

        // Direct Bash already passed the host's explicit command ingress and
        // Native sends no PromptSubmit event for it. Preserve its source rows
        // exactly instead of feeding them through ordinary prompt screening.
        let direct_bash_ingress = queued_inputs.as_ref().is_some_and(|inputs| {
            inputs.iter().filter(|input| !input.is_meta).count() == 1
                && inputs.iter().filter(|input| !input.is_meta).all(|input| {
                    input.prompt_ingress_kind
                        == Some(crate::request_context::PromptIngressKind::DirectBash)
                })
                && inputs.iter().all(|input| {
                    input.is_meta || {
                        input.prompt_ingress_kind
                            == Some(crate::request_context::PromptIngressKind::DirectBash)
                    }
                })
        });
        let direct_bash_no_query = direct_bash_ingress
            && queued_inputs
                .as_ref()
                .is_some_and(|inputs| inputs.iter().any(|input| input.skip_model_query));
        // Screen each ordinary submission before a user row is committed.
        let mut mod_contexts = Vec::new();
        let mut screened_prompt = None;
        let queued_inputs = if transient_rewake || direct_bash_ingress {
            queued_inputs
        } else {
            match queued_inputs {
                Some(inputs) => {
                    let mut admitted = Vec::with_capacity(inputs.len());
                    let image_owner = inputs.iter().position(|input| !input.is_meta).unwrap_or(0);
                    for (index, mut input) in inputs.into_iter().enumerate() {
                        if let Some(kind) =
                            input.mod_origin.as_ref().and_then(external_receive_kind)
                        {
                            let Some(received) = orch
                                .screen_mod_queued_receive_source(&input.text, kind)
                                .await
                            else {
                                if index == image_owner {
                                    images.clear();
                                }
                                continue;
                            };
                            input.text = received;
                        }
                        match orch
                            .screen_mod_prompt_submit(
                                &input.text,
                                if index == image_owner { &images } else { &[] },
                                input.mod_origin.clone(),
                            )
                            .await
                        {
                            ModPromptScreen::Admit { text, context } => {
                                input.text = text;
                                admitted.push(input);
                                mod_contexts.push(context);
                            }
                            ModPromptScreen::Drop(reason) => {
                                if index == image_owner {
                                    images.clear();
                                }
                                orch.emit_mod_prompt_drop(&reason).await;
                            }
                        }
                    }
                    if admitted.is_empty() {
                        return Ok(ConversationOutcome::StopHookPrevented {
                            turn_count: 0,
                            final_message_id: message_id.unwrap_or_else(MessageId::new),
                        });
                    }
                    Some(admitted)
                }
                None => {
                    let origin = crate::mod_prompt_origin::current();
                    let original = projected_content
                        .as_ref()
                        .map(|content| content.text.clone())
                        .unwrap_or_else(|| super::source_string_projection(prompt));
                    let mut received = original.clone();
                    if let Some(kind) = external_receive_kind(&origin) {
                        let Some(text) =
                            orch.screen_mod_queued_receive_source(&original, kind).await
                        else {
                            return Ok(ConversationOutcome::StopHookPrevented {
                                turn_count: 0,
                                final_message_id: message_id.unwrap_or_else(MessageId::new),
                            });
                        };
                        received = text;
                    }
                    if let Some(content) = &mut projected_content {
                        content.rewrite_text(received.clone())?;
                    }
                    match orch
                        .screen_mod_prompt_submit_projected(&received, &images, None)
                        .await
                    {
                        ModPromptScreen::Admit { text, context } => {
                            if let Some(content) = &mut projected_content {
                                content.rewrite_text(text.clone())?;
                            }
                            screened_prompt = Some(text);
                            mod_contexts.push(context);
                        }
                        ModPromptScreen::Drop(reason) => {
                            orch.emit_mod_prompt_drop(&reason).await;
                            return Ok(ConversationOutcome::StopHookPrevented {
                                turn_count: 0,
                                final_message_id: message_id.unwrap_or_else(MessageId::new),
                            });
                        }
                    }
                    None
                }
            }
        };
        let prompt_projection =
            screened_prompt.unwrap_or_else(|| super::source_string_projection(prompt));
        let sdk_content_is_json_string = projected_content
            .as_ref()
            .map(|content| content.source_was_json_string);

        // 1. Append the user prompt (+ any pasted images) to session history.
        // `images` arrives already decoded (path-based callers ran `load_images`
        // first; the bridge converts inline `ImageRefDto`s straight to sources).
        let mut reused_unanswered_user_row = false;
        let submissions: Vec<(QueuedPromptInput, ConversationMessage)> = match queued_inputs {
            Some(inputs) => {
                let mut submissions = Vec::with_capacity(inputs.len());
                for input in inputs {
                    let mut message = super::user_message_from_projection(
                        input.message_id.unwrap_or_default(),
                        &input.text,
                        input.is_meta,
                        input.images.clone(),
                    );
                    if input.prompt_ingress_kind
                        == Some(crate::request_context::PromptIngressKind::DirectBash)
                    {
                        if let ConversationMessage::User { content, .. } = &mut message {
                            // Native carries pasted input blocks before the
                            // wrapped Bash command. `user_with_images` builds
                            // text before images, so restore Native's order
                            // for this host-stamped ingress before persistence
                            // and request normalization.
                            let mut preceding = Vec::with_capacity(content.len());
                            let mut command = Vec::with_capacity(content.len());
                            for block in content.drain(..) {
                                if matches!(&block, lingxi_core::types::ContentBlock::Image { .. })
                                {
                                    preceding.push(block);
                                } else {
                                    command.push(block);
                                }
                            }
                            preceding.extend(command);
                            *content = preceding;
                        }
                    }
                    if input.turn_companion {
                        if let ConversationMessage::User { turn_companion, .. } = &mut message {
                            *turn_companion = Some(Some(true));
                        }
                    }
                    orch.apply_native_user_message_source(
                        &mut message,
                        &crate::native_user_message_source::NativeUserMessageSource {
                            message_origin: input.message_origin.clone(),
                            prompt_source: input.prompt_source.clone(),
                        },
                    )
                    .await;
                    submissions.push((input, message));
                }
                submissions
            }
            None => {
                let resumed_unanswered_user = if in_human_turn && !transient_rewake {
                    if let Some(message_id) = message_id {
                        let session = orch.session.lock().await;
                        session
                            .message_turn_is_unanswered(message_id)
                            .then(|| {
                                session
                                    .history
                                    .iter()
                                    .find(|message| {
                                        message.id() == message_id
                                            && matches!(
                                                message,
                                                ConversationMessage::User { is_meta: false, .. }
                                            )
                                    })
                                    .cloned()
                            })
                            .flatten()
                    } else {
                        None
                    }
                } else {
                    None
                };
                let mut message = match resumed_unanswered_user {
                    Some(message) => {
                        reused_unanswered_user_row = true;
                        message
                    }
                    None => super::user_message_from_projection(
                        message_id.unwrap_or_default(),
                        &prompt_projection,
                        !in_human_turn,
                        images.clone(),
                    ),
                };
                if !reused_unanswered_user_row {
                    orch.apply_native_user_message_source(
                        &mut message,
                        &crate::native_user_message_source::current().unwrap_or_default(),
                    )
                    .await;
                    if let (Some(projected), ConversationMessage::User { content, .. }) =
                        (projected_content, &mut message)
                    {
                        *content = projected.blocks;
                    }
                }
                vec![(
                    QueuedPromptInput {
                        goal_retry_id: None,
                        text: prompt_projection.clone(),
                        images,
                        is_meta: !in_human_turn,
                        sdk_content_is_json_string,
                        message_id,
                        transcript_row_token: row_token,
                        ..Default::default()
                    },
                    message,
                )]
            }
        };
        // A human entry owns turn-level UI/hook identity, but never rewrites a
        // neighboring scheduled entry's per-message metadata.
        let primary = submissions
            .iter()
            .position(|(input, _)| !input.is_meta)
            .unwrap_or(0);
        let user_msg = &submissions[primary].1;
        let empty_mod_turn_text = super::source_string_projection("");
        let mod_turn_text = if submissions[primary].0.is_meta {
            &empty_mod_turn_text
        } else {
            &submissions[primary].0.text
        };
        let prior_message_id = {
            orch.session
                .lock()
                .await
                .history
                .last()
                .map(ConversationMessage::id)
        };
        if !transient_rewake {
            let has_native_skill_companion =
                submissions.iter().any(|(input, _)| input.turn_companion);
            if has_native_skill_companion {
                // Native persists the command header and expanded body as two
                // adjacent user rows before the hook-yield rows. Keep all user
                // rows durable first; then attach request context and the
                // body's ordered hook history rows.
                for (input, message) in &submissions {
                    if let Some(identity) = &input.pending_prompt_identity {
                        let persisted_message = orch
                            .persist_pending_prompt_queued_message_once(
                                message,
                                input,
                                &identity.prompt_id,
                            )
                            .await?;
                        orch.append_pending_prompt_message_to_history_once(&persisted_message)
                            .await?;
                    } else {
                        orch.session.lock().await.history.push(message.clone());
                        orch.persist_queued_message_to_jsonl(message, input).await;
                    }
                }
                for (index, ((input, message), _)) in
                    submissions.iter().zip(&mod_contexts).enumerate()
                {
                    if index == primary && !input.is_meta {
                        crate::request_context::persist_prompt_ingress_after_user(
                            orch,
                            message.id(),
                            input
                                .prompt_ingress_kind
                                .unwrap_or(crate::request_context::PromptIngressKind::LocalSkill),
                        )
                        .await?;
                    }
                }
                for (input, _) in &submissions {
                    if let Some(identity) = &input.pending_prompt_identity {
                        orch.append_native_prompt_expansion_history_rows_once(
                            &input.trailing_history_rows,
                            &identity.trailing_history_row_ids,
                        )
                        .await?;
                    } else {
                        orch.append_native_prompt_expansion_history_rows(
                            &input.trailing_history_rows,
                        )
                        .await;
                    }
                }
                for context in &mod_contexts {
                    orch.append_mod_prompt_context(context).await;
                }
            } else if direct_bash_ingress {
                // Direct Bash bypasses ordinary prompt screening above, so it
                // has no ModPromptScreen contexts. Persist each Native row
                // directly instead of zipping submissions with that empty
                // context list. Its trailing rows are the Bash output messages,
                // not hook attachments; retain their checkpointed UUIDs when
                // available and append source-preserving User rows after input.
                for (input, message) in &submissions {
                    if let Some(identity) = &input.pending_prompt_identity {
                        let persisted_message = orch
                            .persist_pending_prompt_queued_message_once(
                                message,
                                input,
                                &identity.prompt_id,
                            )
                            .await?;
                        orch.append_pending_prompt_message_to_history_once(&persisted_message)
                            .await?;
                    } else {
                        orch.session.lock().await.history.push(message.clone());
                        orch.persist_queued_message_to_jsonl(message, input).await;
                    }
                }
                for (input, _) in &submissions {
                    let identity = input.pending_prompt_identity.as_ref();
                    if let Some(identity) = identity {
                        if input.trailing_history_rows.len()
                            != identity.trailing_history_row_ids.len()
                        {
                            return Err(OrchestratorError::Internal(format!(
                                "direct Bash trailing row identity count mismatch: {} rows, {} ids",
                                input.trailing_history_rows.len(),
                                identity.trailing_history_row_ids.len(),
                            )));
                        }
                    }
                    for (index, row) in input.trailing_history_rows.iter().enumerate() {
                        if let Some(identity) = identity {
                            let row_id = identity.trailing_history_row_ids[index];
                            let message =
                                super::user_message_from_projection(row_id, row, false, Vec::new());
                            let row_input = QueuedPromptInput {
                                text: row.clone(),
                                message_id: Some(row_id),
                                pending_prompt_identity: Some(super::PendingPromptIdentity {
                                    prompt_id: identity.prompt_id.clone(),
                                    trailing_history_row_ids: Vec::new(),
                                }),
                                ..QueuedPromptInput::default()
                            };
                            let persisted_message = orch
                                .persist_pending_prompt_queued_message_once(
                                    &message,
                                    &row_input,
                                    &identity.prompt_id,
                                )
                                .await?;
                            orch.append_pending_prompt_message_to_history_once(&persisted_message)
                                .await?;
                        } else {
                            let row_id = lingxi_core::types::MessageId::new();
                            let message =
                                super::user_message_from_projection(row_id, row, false, Vec::new());
                            let row_input = QueuedPromptInput {
                                text: row.clone(),
                                message_id: Some(row_id),
                                ..QueuedPromptInput::default()
                            };
                            orch.session.lock().await.history.push(message.clone());
                            orch.persist_queued_message_to_jsonl(&message, &row_input)
                                .await;
                        }
                    }
                }
            } else {
                for (index, ((input, message), context)) in
                    submissions.iter().zip(&mod_contexts).enumerate()
                {
                    if !reused_unanswered_user_row {
                        if let Some(identity) = &input.pending_prompt_identity {
                            let persisted_message = orch
                                .persist_pending_prompt_queued_message_once(
                                    message,
                                    input,
                                    &identity.prompt_id,
                                )
                                .await?;
                            orch.append_pending_prompt_message_to_history_once(&persisted_message)
                                .await?;
                        } else {
                            orch.session.lock().await.history.push(message.clone());
                            orch.persist_queued_message_to_jsonl(message, input).await;
                        }
                    }
                    if index == primary && !input.is_meta {
                        crate::request_context::persist_prompt_ingress_after_user(
                            orch,
                            message.id(),
                            input.prompt_ingress_kind.unwrap_or_else(|| {
                                crate::request_context::prompt_ingress_kind_for_user_submission(
                                    false,
                                )
                            }),
                        )
                        .await?;
                    }
                    if let Some(identity) = &input.pending_prompt_identity {
                        orch.append_native_prompt_expansion_history_rows_once(
                            &input.trailing_history_rows,
                            &identity.trailing_history_row_ids,
                        )
                        .await?;
                    } else {
                        orch.append_native_prompt_expansion_history_rows(
                            &input.trailing_history_rows,
                        )
                        .await;
                    }
                    orch.append_mod_prompt_context(context).await;
                }
            }
        }
        if !transient_rewake && direct_bash_ingress {
            crate::request_context::persist_prompt_ingress_after_user(
                orch,
                user_msg.id(),
                crate::request_context::PromptIngressKind::DirectBash,
            )
            .await?;
        }
        // (/rewind) Snapshot the pre-turn file state IN MEMORY, keyed by this
        // user message, so `track_edit` (fired by Edit/Write/NotebookEdit during
        // the turn) records each file's pre-edit backup into it. The POPULATED
        // record is persisted to the transcript at TURN END (see below) — NOT
        // here: at turn start the backup map is empty (no edits yet), and
        // persisting it now would leave disk-based restore (`rewind_from_disk`,
        // which runs after the TUI unwinds and rebuilds the index from these
        // lines) with nothing to restore.
        let file_history_msg_id = (!transient_rewake).then(|| user_msg.id().as_uuid());
        if let (Some(fh), Some(message_id)) = (&orch.file_history, file_history_msg_id) {
            fh.make_snapshot(message_id).await;
        }

        // A completed DirectBash with response disabled, or an interrupted
        // command, persists its Native rows and ends before any model request.
        if direct_bash_no_query {
            let final_message_id = user_msg.id();
            orch.emit_turn_terminal("end_turn", &orch.snapshot_cost_real().await)
                .await;
            return Ok(ConversationOutcome::EndTurn {
                turn_count: 0,
                final_message_id,
            });
        }

        // hooks B4: UserPromptSubmit (streaming twin). A Block aborts before the
        // first stream is opened. No-op when unregistered.
        if !transient_rewake {
            for (input, message) in &submissions {
                // Synthetic entries (cron fires, `/loop` wakeups) are not user
                // prompts: firing the hook for them double-counts telemetry and
                // re-runs `append_ultracode_attachments` per batch entry.
                if input.is_meta
                    || input.prompt_ingress_kind
                        == Some(crate::request_context::PromptIngressKind::DirectBash)
                {
                    continue;
                }
                let prompt_projection = super::projected_content::exact_message_text(message);
                if orch
                    .fire_user_prompt_submit_projected(&prompt_projection, message.id())
                    .await?
                {
                    return Ok(ConversationOutcome::StopHookPrevented {
                        turn_count: 0,
                        final_message_id: message.id(),
                    });
                }
            }
        }

        orch.begin_output_turn(user_msg.id()).await?;
        let mut loop_state = TurnLoopState::new(
            orch,
            prior_message_id.unwrap_or_else(|| user_msg.id()),
            user_cancel.clone(),
        );
        let final_message_id;
        let mut natural_terminal_pending = false;
        let mut forced_terminal_pending = None;
        let mut final_drain_continuation = false;
        let mut deferred_ack_error = None;
        loop {
            // MID-TURN DRAIN (claude-code query.ts ~1570-1580): drain BEFORE
            // every terminal top-of-loop guard, including `max_turns`. A message
            // can arrive while the previous model/tool step is running; returning
            // for the cap before this consume-once source is polled would discard
            // it. Claude Code 2.1.205 preserves that message when `--max-turns`
            // ends the turn, so inject it into the persisted history first.
            // With no source wired this remains a strict no-op.
            // The cancel guard is NOT part of this: on this path it runs AFTER
            // the increment (below), where the two batched entries have it first
            // or not at all.
            if final_drain_continuation && token_aborted(&user_cancel) {
                let cost = orch.snapshot_cost_real().await;
                orch.emit_turn_terminal("aborted_streaming", &cost).await;
                orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                    .await;
                final_message_id = loop_state.last_message_id;
                break;
            }
            // This marker is set only by a failed source acknowledgement;
            // unrelated Internal errors must keep their original return path.
            let mut ack_failure = false;
            let guard_verdict = match orch
                .run_turn_loop_guards_with_cancel_drain(
                    loop_state::LoopGuardOrder::Streaming,
                    &mut loop_state,
                    final_drain_continuation,
                    &mut ack_failure,
                )
                .await
            {
                Err(error)
                    if ack_failure
                        || (matches!(&error, OrchestratorError::RecoveryInterrupted)
                            && token_aborted(&user_cancel)) =>
                {
                    deferred_ack_error = Some(error);
                    final_message_id = loop_state.last_message_id;
                    break;
                }
                Err(error) => return Err(error),
                Ok(verdict) => verdict,
            };
            if final_drain_continuation
                && token_aborted(&user_cancel)
                && !matches!(
                    &guard_verdict,
                    loop_state::GuardVerdict::StructuredOutputRetries(_)
                )
            {
                let cost = orch.snapshot_cost_real().await;
                orch.emit_turn_terminal("aborted_streaming", &cost).await;
                orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                    .await;
                final_message_id = loop_state.last_message_id;
                break;
            }
            match guard_verdict {
                loop_state::GuardVerdict::Proceed => final_drain_continuation = false,
                loop_state::GuardVerdict::StructuredOutputRetries(error) => return Err(error),
                loop_state::GuardVerdict::MaxTurns => {
                    let cost = orch.snapshot_cost_real().await;
                    if final_drain_continuation && token_aborted(&user_cancel) {
                        orch.emit_turn_terminal("aborted_streaming", &cost).await;
                        orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                            .await;
                        final_message_id = loop_state.last_message_id;
                        break;
                    }
                    return Err(OrchestratorError::MaxTurnsReached {
                        max_turns: orch.config.max_turns,
                    });
                }
                loop_state::GuardVerdict::OverBudget => {
                    return Err(OrchestratorError::MaxBudgetReached {
                        budget_nano_usd: orch.config.max_budget_nano_usd.unwrap_or(0),
                    });
                }
            }

            // DEFERRED-3 / esc-interrupt FIX: top-of-loop user-interrupt guard
            // (faithful port of claude-code `query.ts:1015` — the `aborted_streaming`
            // return). If the user-interrupt token is already set when we reach the
            // top of an iteration — a pre-cancel, or an abort that fired during the
            // previous iteration's streaming BEFORE any tool ran — we must STOP
            // BEFORE issuing the next `callModel`. claude-code consumes any
            // remaining streaming results then returns `aborted_streaming` with no
            // further sampling; here the previous iteration already drained its
            // results into history (the post-tools guard) or there were none, so we
            // simply break. This is the structural barrier that prevents a
            // Block-behavior tool on a post-interrupt continuation from ever
            // executing. `None` token → never fires → identical to before.
            if user_cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                let cost = orch.snapshot_cost_real().await;
                orch.emit_turn_terminal("aborted_streaming", &cost).await;
                // The persistence boundary filters non-user cancellations.
                orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                    .await;
                final_message_id = loop_state.last_message_id;
                break;
            }

            if !loop_state.mod_turn_started {
                orch.fire_mod_turn_start(mod_turn_text, &loop_state.mod_turn_id)
                    .await;
                loop_state.mod_turn_started = true;
            }

            let round = Box::pin(Self::run_round(
                orch,
                &system_prompt,
                &user_cancel,
                &mut loop_state,
                in_human_turn,
                &mut ack_failure,
            ))
            .await;
            let round = match round {
                Err(error)
                    if ack_failure
                        || (matches!(&error, OrchestratorError::RecoveryInterrupted)
                            && token_aborted(&user_cancel)) =>
                {
                    deferred_ack_error = Some(error);
                    final_message_id = loop_state.last_message_id;
                    break;
                }
                Err(error) => return Err(error),
                Ok(round) => round,
            };
            match round {
                StepExit::Continue => continue,
                StepExit::ContinueAfterFinalDrain => {
                    final_drain_continuation = true;
                    continue;
                }
                StepExit::FinishThroughEpilogue(message_id) => {
                    final_message_id = message_id;
                    break;
                }
                StepExit::FinishNaturalThroughEpilogue(message_id) => {
                    final_message_id = message_id;
                    natural_terminal_pending = true;
                    break;
                }
                StepExit::FinishForcedThroughEpilogue(message_id, terminal) => {
                    final_message_id = message_id;
                    forced_terminal_pending = Some(terminal);
                    break;
                }
                // §3.6: skips the file-history epilogue below, which is exactly
                // what `Return` has always done.
                StepExit::ReturnDirect(outcome) => return Ok(outcome),
            }
        }

        // (/rewind) Persist THIS turn's now-populated file-history snapshot to the
        // transcript — `track_edit` filled its backup map (pre-edit content of
        // every file Edit/Write/NotebookEdit touched) during the turn above.
        // Restore (`rewind_from_disk`) and `--resume` rebuild the index from
        // these lines, so the record must carry the backups, not the empty map
        // it had at turn start. Persisted unconditionally (an edit-free turn
        // still records a restore point for conversation-only rewind).
        if let (Some(fh), Some(file_history_msg_id)) = (&orch.file_history, file_history_msg_id) {
            if let (Some(record), Some(writer)) = (
                fh.snapshot_record(file_history_msg_id),
                &orch.transcript.jsonl_writer,
            ) {
                let session_id = orch.session.lock().await.session_id;
                let session_uuid = session_id.as_uuid().to_string();
                let line = session::file_history::snapshot_line_json(&session_uuid, &record);
                if let Err(error) = writer.append_file_history_snapshot(&line).await {
                    orch.record_transcript_append_failure(
                        &session_id.to_string(),
                        "file_history_snapshot",
                        &error,
                    )
                    .await;
                }
            }
        }

        // A failed acknowledgement is not a successful turn. Preserve the
        // original error after the ordinary file-history epilogue so the owner
        // can keep the claimed input uncertain and finish cleanup.
        if let Some(error) = deferred_ack_error {
            return Err(error);
        }

        // The final response can cross the session budget without requiring
        // another loop iteration. Check after owned tools, accounting and
        // persistence have settled, so an end_turn cannot bypass the limit.
        let over_budget = !token_aborted(&user_cancel) && orch.over_budget().await;
        if over_budget && !token_aborted(&user_cancel) {
            return Err(OrchestratorError::MaxBudgetReached {
                budget_nano_usd: orch.config.max_budget_nano_usd.unwrap_or(0),
            });
        }

        if natural_terminal_pending {
            let cost = orch.snapshot_cost_real().await;
            if token_aborted(&user_cancel) {
                orch.emit_turn_terminal("aborted_streaming", &cost).await;
                orch.inject_query_user_interruption(QueryUserInterruptionKind::Streaming)
                    .await;
            } else {
                orch.emit_turn_terminal("end_turn", &cost).await;
            }
        } else if let Some(terminal) = forced_terminal_pending {
            orch.publish_deferred_terminal(terminal, &user_cancel).await;
        }

        Ok(ConversationOutcome::EndTurn {
            turn_count: loop_state.turn_count,
            final_message_id,
        })
    }
}
