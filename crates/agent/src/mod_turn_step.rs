//! The child runner's `turn.step` stream bridge.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt as _};
use hooks::mods::{ModError, ModHost, ModStreamSource, ModUtf16ValueProjection};
use lingxi_core::types::ConversationMessage;
use llm_runtime::mod_turn_step::{TurnStepDecoder, TurnStepEncoder};
use llm_runtime::{HistoryEvent, HistoryResponse, LlmError};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::api::{SubagentApiCallOpts, SubagentApiClient, SubagentApiRequest};

#[derive(Clone)]
pub(crate) struct ChildStepRequest {
    pub api: Arc<dyn SubagentApiClient>,
    pub model: String,
    pub profile: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    pub effort: Option<Value>,
    pub forced_tool: Option<String>,
    pub call_opts: SubagentApiCallOpts,
    pub fallback_target: lingxi_core::host::refusal_driver::FallbackTargetContext,
    /// Trusted spawn provenance for this child turn's host-dispatched Mod
    /// hooks. It is never serialized into the model-facing `turn.step` input.
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
}

impl ChildStepRequest {
    pub async fn open(
        &self,
        model: &str,
        effort: Option<Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        lingxi_core::host::refusal_driver::scope_fallback_target(
            self.fallback_target.clone(),
            self.api.stream(SubagentApiRequest {
                model: model.to_owned(),
                profile: self.profile.clone(),
                system: self.system.clone(),
                messages: self.messages.clone(),
                tools: self.tools.clone(),
                forced_tool: self.forced_tool.clone(),
                effort,
                opts: self.call_opts.clone(),
            }),
        )
        .await
    }
}

type Frame = (Result<HistoryEvent, LlmError>, oneshot::Sender<()>);

async fn send_events(
    sender: &mpsc::Sender<Frame>,
    events: Vec<HistoryEvent>,
) -> Result<(), ModError> {
    for event in events {
        let terminal = matches!(
            event,
            HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
        );
        let (ack, received) = oneshot::channel();
        sender
            .send((Ok(event), ack))
            .await
            .map_err(|_| ModError::Unavailable("child turn.step consumer closed".into()))?;
        if !terminal {
            received
                .await
                .map_err(|_| ModError::Unavailable("child turn.step consumer closed".into()))?;
        }
    }
    Ok(())
}

pub(crate) fn stream(
    host: Arc<ModHost>,
    input: Value,
    request: ChildStepRequest,
    cwd: PathBuf,
    physical_responses: Arc<Mutex<Vec<HistoryResponse>>>,
) -> BoxStream<'static, Result<HistoryEvent, LlmError>> {
    let wire = Arc::new(Mutex::new(TurnStepEncoder::default()));
    let decoder = Arc::new(Mutex::new(TurnStepDecoder::new(&request.model)));
    let inherited_hook_origin = request.agent_spawn_provenance.hook_origin.clone();
    let physical_error = Arc::new(Mutex::new(None::<LlmError>));
    let requested_model = Arc::new(AtomicBool::new(false));
    let pending_output = Arc::new(Mutex::new(Vec::<HistoryEvent>::new()));
    let (sender, receiver) = mpsc::channel::<Frame>(1);
    tokio::spawn(async move {
        let fallback_request = request.clone();
        let fallback_physical = physical_responses.clone();
        let source_wire = wire.clone();
        let source_error = physical_error.clone();
        let source_requested = requested_model.clone();
        let policy_host = host.clone();
        let source = move |forwarded: Value| {
            let request = request.clone();
            let host = policy_host.clone();
            let wire = source_wire.clone();
            let physical_error = source_error.clone();
            let physical_responses = physical_responses.clone();
            let requested_model = source_requested.clone();
            async move {
                let requested = forwarded["model"]
                    .as_str()
                    .unwrap_or(&request.model)
                    .to_owned();
                let route = request
                    .api
                    .resolve_mod_media_route(&requested, request.profile.as_deref())
                    .or_else(|| request.api.resolve_mod_media_route(&requested, None));
                let resolved_model = route.as_ref().map_or(requested.as_str(), |route| {
                    route.main.request_model.as_str()
                });
                let denied = host.model_allowed(resolved_model).await? == Some(false);
                let model = if denied {
                    tracing::warn!(requested, "child turn.step model rewrite denied by policy");
                    request.model.clone()
                } else {
                    requested
                };
                let mut effective_request = request.clone();
                if !denied {
                    effective_request.profile = route
                        .map(|route| route.main.profile_name)
                        .or(request.profile.clone());
                }
                let effort = forwarded.get("effort").cloned().or(request.effort.clone());
                requested_model.store(true, Ordering::Release);
                let provider = effective_request
                    .open(&model, effort)
                    .await
                    .map_err(|error| {
                        *physical_error.lock().unwrap() = Some(error.clone());
                        ModError::Hook(error.to_string())
                    })?;
                let refs = Arc::new(Mutex::new(Vec::new()));
                let chunk_refs = refs.clone();
                let chunk_wire = wire.clone();
                let result_wire = wire.clone();
                let response_events = Arc::new(Mutex::new(Vec::new()));
                let provider = provider.then(move |item| {
                    let response_events = response_events.clone();
                    let physical_responses = physical_responses.clone();
                    let chunk_wire = chunk_wire.clone();
                    let chunk_refs = chunk_refs.clone();
                    let physical_error = physical_error.clone();
                    async move {
                        match item {
                            Ok(event) => {
                                let completed = matches!(event, HistoryEvent::Completed { .. });
                                let stopped = matches!(event, HistoryEvent::MessageStop);
                                let mut streamed_response = None;
                                response_events.lock().unwrap().push(event.clone());
                                if completed || stopped {
                                    let events = std::mem::take(&mut *response_events.lock().unwrap());
                                    if let Ok(response) = llm_runtime::stream_accumulator::accumulate_stream_salvaging(
                                        stream::iter(events.into_iter().map(Ok)).boxed(),
                                    ).await {
                                        if stopped {
                                            streamed_response = Some(response.clone());
                                        }
                                        physical_responses.lock().unwrap().push(response);
                                    }
                                }
                                let held = {
                                    let mut wire = chunk_wire.lock().unwrap();
                                    let held = wire.push(event).clone();
                                    if let Some(response) = streamed_response {
                                        wire.note_stream_response(held.reference, response);
                                    }
                                    held
                                };
                                chunk_refs.lock().unwrap().push(held.reference);
                                Ok(ModUtf16ValueProjection::from_core_projection(held.chunk)?)
                            }
                            Err(error) => {
                                *physical_error.lock().unwrap() = Some(error.clone());
                                Err(ModError::Hook(error.to_string()))
                            }
                        }
                    }
                });
                let turn_id = forwarded["turnId"].as_str().unwrap_or_default().to_owned();
                let index = forwarded["index"].as_u64().unwrap_or_default() as u32;
                Ok(ModStreamSource::new(provider, move || {
                    let refs = refs.lock().unwrap().clone();
                    let result = result_wire
                        .lock()
                        .unwrap()
                        .result_for_references(&turn_id, index, &refs);
                    ModUtf16ValueProjection::from_core_projection(result)
                }))
            }
        };
        let output_wire = wire.clone();
        let output_decoder = decoder.clone();
        let output_sender = sender.clone();
        let output_requested = requested_model.clone();
        let output_pending = pending_output.clone();
        let on_chunk = move |chunk: ModUtf16ValueProjection| {
            let wire = output_wire.clone();
            let decoder = output_decoder.clone();
            let sender = output_sender.clone();
            let requested_model = output_requested.clone();
            let pending_output = output_pending.clone();
            async move {
                let chunk = chunk.into_core_projection()?;
                let events = {
                    let wire = wire.lock().unwrap();
                    decoder.lock().unwrap().consume(&chunk, &wire)
                };
                if !requested_model.load(Ordering::Acquire) {
                    // Before a Mod reaches `next(e)`, its output is provisional:
                    // if the hook then fails, Native retries the unchanged physical
                    // request and discards every yielded synthetic block. Hold the
                    // complete event prefix until the source is opened or dispatch
                    // completes. The success path flushes this buffer (preserving
                    // synthetic-only answers); the pre-source failure path drops it
                    // before forwarding the original stream.
                    pending_output.lock().unwrap().extend(events);
                    return Ok(());
                }
                let mut ready = std::mem::take(&mut *pending_output.lock().unwrap());
                ready.extend(events);
                send_events(&sender, ready).await
            }
        };
        let result = host
            .dispatch_turn_step_stream_at_and_origin(
                input,
                &cwd,
                inherited_hook_origin,
                source,
                on_chunk,
            )
            .await;
        match result {
            Ok(_) => {
                let tail = {
                    let wire = wire.lock().unwrap();
                    decoder.lock().unwrap().finish(&wire)
                };
                let mut ready = std::mem::take(&mut *pending_output.lock().unwrap());
                ready.extend(tail);
                let _ = send_events(&sender, ready).await;
            }
            Err(error) => {
                let request_started = requested_model.load(Ordering::Acquire);
                let aborted_before_request = !request_started
                    && matches!(
                        &error,
                        ModError::Hook(message) if message.contains("stream dispatch cancelled")
                    );
                if !request_started && !aborted_before_request {
                    // Native `vs` retries the unmodified model request when a
                    // hook fails before reaching the bottom of the chain.
                    // Native Jn buffers pre-request assistant output and drops that
                    // buffer when the chain throws before its first physical call;
                    // vs then runs the unchanged request without hooks.
                    pending_output.lock().unwrap().clear();
                    let opened = tokio::select! {
                        biased;
                        _ = sender.closed() => return,
                        opened = fallback_request.open(
                            &fallback_request.model,
                            fallback_request.effort.clone(),
                        ) => opened,
                    };
                    match opened {
                        Ok(mut provider) => {
                            let mut physical_events = Vec::new();
                            while let Some(item) = provider.next().await {
                                if let Ok(event) = &item {
                                    physical_events.push(event.clone());
                                    if matches!(
                                        event,
                                        HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
                                    ) {
                                        if let Ok(response) = llm_runtime::stream_accumulator::accumulate_stream_salvaging(
                                            stream::iter(std::mem::take(&mut physical_events).into_iter().map(Ok)).boxed(),
                                        ).await {
                                            fallback_physical.lock().unwrap().push(response);
                                        }
                                    }
                                }
                                let terminal = matches!(
                                    item,
                                    Ok(HistoryEvent::MessageStop | HistoryEvent::Completed { .. })
                                );
                                let (ack, received) = oneshot::channel();
                                if sender.send((item, ack)).await.is_err() {
                                    return;
                                }
                                if !terminal && received.await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let (ack, _) = oneshot::channel();
                            let _ = sender.send((Err(error), ack)).await;
                        }
                    }
                    return;
                }
                if aborted_before_request {
                    // Native drops the pre-request buffer on abort, but unlike
                    // an ordinary hook failure it does not retry the physical call.
                    pending_output.lock().unwrap().clear();
                } else {
                    let pending = std::mem::take(&mut *pending_output.lock().unwrap());
                    if send_events(&sender, pending).await.is_err() {
                        return;
                    }
                }
                let typed = physical_error.lock().unwrap().take().unwrap_or_else(|| {
                    LlmError::StreamInterrupted {
                        message: error.to_string(),
                    }
                });
                let (ack, _) = oneshot::channel();
                let _ = sender.send((Err(typed), ack)).await;
            }
        }
    });
    stream::unfold(
        (receiver, None::<oneshot::Sender<()>>),
        |(mut receiver, previous_ack)| async move {
            if let Some(ack) = previous_ack {
                let _ = ack.send(());
            }
            let (item, ack) = receiver.recv().await?;
            Some((item, (receiver, Some(ack))))
        },
    )
    .boxed()
}

pub(crate) fn input(
    turn_id: &str,
    index: u32,
    agent_id: &str,
    model: &str,
    effort: Option<&Value>,
    message_count: usize,
) -> Value {
    let mut input = json!({
        "turnId":turn_id,
        "index":index,
        "agentId":agent_id,
        "model":model,
        "messageCount":message_count,
    });
    if let Some(effort) = effort {
        input["effort"] = effort.clone();
    }
    input
}
