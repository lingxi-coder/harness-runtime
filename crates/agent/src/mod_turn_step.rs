//! The child runner's `turn.step` stream bridge.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt as _};
use hooks::mods::{ModError, ModHost, ModStreamSource};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ConversationMessage;
use llm_runtime::mod_turn_step::{TurnStepDecoder, TurnStepEncoder};
use llm_runtime::{HistoryEvent, HistoryResponse, LlmError, ToolDeclaration};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::api::{SubagentApiCallOpts, SubagentApiClient, SubagentApiRequest};

#[derive(Clone)]
pub(crate) struct ChildStepRequest {
    pub api: Arc<dyn SubagentApiClient>,
    pub model: String,
    pub profile: Option<String>,
    pub system: Option<Utf16JsonProjection>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolDeclaration>,
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
    input: Utf16JsonProjection,
    request: ChildStepRequest,
    cwd: PathBuf,
    session: Option<Arc<dyn hooks::mods::ModSessionContext>>,
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
        let source = move |forwarded: Utf16JsonProjection| {
            let request = request.clone();
            let host = policy_host.clone();
            let wire = source_wire.clone();
            let physical_error = source_error.clone();
            let physical_responses = physical_responses.clone();
            let requested_model = source_requested.clone();
            async move {
                let requested = forwarded
                    .string_at("/model")
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
                let effort = if let Ok(source) = forwarded.subprojection("/effort") {
                    let json = source.to_json_string().map_err(|error| {
                        ModError::Hook(format!("turn.step effort serialization failed: {error}"))
                    })?;
                    Some(serde_json::from_str(&json).map_err(|error| {
                        ModError::Hook(format!("turn.step effort decoding failed: {error}"))
                    })?)
                } else {
                    request.effort.clone()
                };
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
                                Ok(held.chunk)
                            }
                            Err(error) => {
                                *physical_error.lock().unwrap() = Some(error.clone());
                                Err(ModError::Hook(error.to_string()))
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
                    .filter(|value| {
                        value.is_finite()
                            && *value >= 0.0
                            && value.fract() == 0.0
                            && *value < u64::MAX as f64
                    })
                    .map(|value| value as u64 as u32)
                    .unwrap_or_default();
                Ok(ModStreamSource::new(provider, move || {
                    let refs = refs.lock().unwrap().clone();
                    let result = result_wire
                        .lock()
                        .unwrap()
                        .result_for_references(&turn_id, index, &refs);
                    Ok(result)
                }))
            }
        };
        let output_wire = wire.clone();
        let output_decoder = decoder.clone();
        let output_sender = sender.clone();
        let output_requested = requested_model.clone();
        let output_pending = pending_output.clone();
        let on_chunk = move |chunk: Utf16JsonProjection| {
            let wire = output_wire.clone();
            let decoder = output_decoder.clone();
            let sender = output_sender.clone();
            let requested_model = output_requested.clone();
            let pending_output = output_pending.clone();
            async move {
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
            .dispatch_turn_step_stream_at_and_origin_with_session(
                input,
                &cwd,
                inherited_hook_origin,
                session,
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
) -> Utf16JsonProjection {
    let input = json!({
        "turnId":turn_id,
        "index":index,
        "agentId":agent_id,
        "model":model,
        "messageCount":message_count,
    });
    let mut input = Utf16JsonProjection::from_serde_value(&input)
        .expect("generated turn.step input is a valid JSON projection");
    if let Some(effort) = effort {
        input
            .set_field(
                "effort",
                Utf16JsonProjection::from_serde_value(effort)
                    .expect("generated effort control is valid JSON"),
            )
            .expect("turn.step input is an object");
    }
    input
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lingxi_core::host::subagent_spawn::{SessionOriginCapability, SessionOriginError};
    use lingxi_core::types::SessionId;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug)]
    pub(crate) struct Origin {
        pub(crate) id: SessionId,
        pub(crate) generation: u64,
        pub(crate) active: Arc<Mutex<(SessionId, u64)>>,
        pub(crate) root: PathBuf,
    }

    #[async_trait::async_trait]
    impl SessionOriginCapability for Origin {
        fn owner_id(&self) -> usize {
            1
        }
        fn session_id(&self) -> SessionId {
            self.id
        }
        fn generation(&self) -> u64 {
            self.generation
        }
        fn cwd(&self) -> &Path {
            &self.root
        }
        fn root(&self) -> &Path {
            &self.root
        }

        async fn admit(&self) -> Result<Arc<dyn Send + Sync>, SessionOriginError> {
            if *self.active.lock().unwrap() != (self.id, self.generation) {
                return Err(SessionOriginError::SessionChanged);
            }
            Ok(Arc::new(()))
        }
    }

    #[derive(Clone)]
    pub(crate) struct Session {
        pub(crate) root: PathBuf,
        pub(crate) origin: Option<Arc<dyn SessionOriginCapability>>,
        pub(crate) model_reads: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl hooks::mods::ModSessionContext for Session {
        fn bind_session_origin(
            &self,
            origin: Arc<dyn SessionOriginCapability>,
        ) -> Result<Option<Arc<dyn hooks::mods::ModSessionContext>>, ModError> {
            if origin.owner_id() != 1 {
                return Ok(None);
            }
            let mut bound = self.clone();
            bound.origin = Some(origin);
            Ok(Some(Arc::new(bound)))
        }

        async fn acquire_session_work_lease(
            &self,
        ) -> Result<Option<Arc<dyn Send + Sync>>, ModError> {
            match &self.origin {
                Some(origin) => origin
                    .admit()
                    .await
                    .map(Some)
                    .map_err(|error| ModError::Unavailable(error.to_string())),
                None => Ok(None),
            }
        }

        fn ui_invalidation_context(&self) -> Option<Arc<dyn hooks::mods::ModSessionContext>> {
            Some(Arc::new(self.clone()))
        }
        fn cwd(&self) -> PathBuf {
            self.root.clone()
        }
        fn root(&self) -> PathBuf {
            self.root.clone()
        }
        async fn model(&self) -> String {
            self.model_reads.fetch_add(1, Ordering::SeqCst);
            "current-session-model".into()
        }
        async fn id(&self) -> String {
            "test-session".into()
        }
        async fn turns(&self) -> u64 {
            0
        }
    }

    struct Provider(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl crate::api::SubagentApiClient for Provider {
        async fn stream(
            &self,
            _request: crate::api::SubagentApiRequest,
        ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let response = HistoryResponse {
                id: "response".into(),
                model: "test-model".into(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "provider answer".into(),
                    cache_control: None,
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                stop_details: None,
                resumable: None,
                usage: llm_runtime::ExecutionUsage::default(),
                cost: None,
                provider_metadata: Default::default(),
            };
            Ok(stream::iter(
                llm_runtime::stream_accumulator::response_to_stream_events(response)
                    .into_iter()
                    .map(Ok),
            )
            .boxed())
        }
    }

    #[tokio::test]
    async fn child_step_keeps_launch_origin_after_switch_and_allows_provider() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("child-step.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
          on('turn.step', async function* ($, e, next) {
            try { await $.session.model(); } catch (_) {}
            const below = next(e);
            for await (const chunk of below) yield chunk;
            return await below.result;
          });
        }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "child-step-origin",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
        )
        .await
        .unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let session: Arc<dyn hooks::mods::ModSessionContext> = Arc::new(Session {
            root: dir.path().to_path_buf(),
            origin: None,
            model_reads: reads.clone(),
        });
        host.attach_background_context(Arc::downgrade(&session));
        let original = SessionId::new();
        let other = SessionId::new();
        let active = Arc::new(Mutex::new((original, 1)));
        let origin: Arc<dyn SessionOriginCapability> = Arc::new(Origin {
            id: original,
            generation: 1,
            active: active.clone(),
            root: dir.path().to_path_buf(),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        for (active_id, generation, capability) in [
            (other, 2, Some(origin.clone())),
            (original, 3, Some(origin.clone())),
            (original, 3, None),
        ] {
            *active.lock().unwrap() = (active_id, generation);
            let bound = host
                .bind_child_turn_step_session(capability, true, dir.path())
                .await
                .unwrap();
            let request = ChildStepRequest {
                api: Arc::new(Provider(calls.clone())),
                model: "test-model".into(),
                profile: None,
                system: None,
                messages: Vec::new(),
                tools: Vec::new(),
                effort: None,
                forced_tool: None,
                call_opts: Default::default(),
                fallback_target: Default::default(),
                agent_spawn_provenance: Default::default(),
            };
            let events = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream(
                    host.clone(),
                    input("child-turn", 0, "agent", "test-model", None, 0),
                    request,
                    dir.path().to_path_buf(),
                    bound,
                    Arc::new(Mutex::new(Vec::new())),
                )
                .collect::<Vec<_>>(),
            )
            .await
            .expect("child step completes");
            assert!(events.iter().all(Result::is_ok), "{events:?}");
        }
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
