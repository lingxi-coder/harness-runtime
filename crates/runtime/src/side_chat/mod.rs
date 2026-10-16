//! Host-owned lightweight conversations. All state and replay stay in RAM.
/// Shared lightweight factory pieces.
pub mod composition;
use async_trait::async_trait;
use client::{
    adapter::{
        AdapterPermissionGate, AskUserQuestionBroker, ClientEventSink, PermissionRequestSink,
    },
    protocol::{
        commands::ClientCommand,
        events::ClientEvent,
        permission::{PermissionRequest, PermissionResponseDto},
        text::ClientTextDto,
    },
};
use command_api::RegistrySlashDispatcher;
use lingxi_core::{
    host::{OrchestratorHandle, SlashDispatchResult},
    types::utf16_json::Utf16JsonProjection,
};
use orchestrator::{ConversationOrchestrator, SideChatParentSnapshot};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

/// Native identity without recursive event metadata.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SideChatInfoDto {
    /// Opaque identity within the current host generation.
    pub side_id: String,
    /// Current admission state: open or closing.
    pub status: String,
}

fn lower_model_source(
    source: Option<&Utf16JsonProjection>,
) -> Result<Option<ClientTextDto>, &'static str> {
    source.map(ClientTextDto::from_projection).transpose()
}

fn split_model_selection_source(
    source: Utf16JsonProjection,
    listings: &[lingxi_core::host::ModelListing],
) -> (
    Utf16JsonProjection,
    Option<lingxi_core::host::ModelRouteAnchor>,
) {
    let Some(units) = source.string_units("") else {
        return (source, None);
    };
    let Ok(text) = String::from_utf16(&units) else {
        return (source, None);
    };
    let trimmed = lingxi_core::host::effort::trim_js_whitespace(&text);
    let (model, profile) = lingxi_core::host::parse_model_ref(&trimmed, listings);
    let route_hint = profile.map(|profile| lingxi_core::host::ModelRouteAnchor {
        model: model.clone(),
        profile: Some(profile),
    });
    if route_hint.is_some() {
        (source, route_hint)
    } else if model != trimmed {
        (Utf16JsonProjection::from(model), route_hint)
    } else {
        (source, route_hint)
    }
}

/// The only composition seam: build a new engine from ready host inputs.
#[async_trait]
pub trait SideChatFactory: Send + Sync {
    /// Build independent session primitives from a fixed parent snapshot.
    async fn build(
        &self,
        snapshot: SideChatParentSnapshot,
        sink: Arc<dyn ClientEventSink>,
        permission_sink: Arc<dyn PermissionRequestSink>,
    ) -> Result<SideChatSession, String>;
}

/// Session primitives must be independent and must not carry durable stores,
/// hooks, MCP, task registries, Mods or background execution services.
pub struct SideChatSession {
    /// Independent RAM-only engine.
    pub orchestrator: Arc<ConversationOrchestrator>,
    /// Filtered ordinary slash and inline-skill dispatcher.
    pub dispatcher: Arc<RegistrySlashDispatcher>,
    /// Independent permission request owner.
    pub permission_gate: Arc<AdapterPermissionGate>,
    /// Independent questionnaire owner.
    pub question_broker: Arc<AskUserQuestionBroker>,
    /// Latched foreground cleanup failure; closing records are retained.
    pub cleanup_failed: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SideChatSession {
    fn drop(&mut self) {
        // The session owns the registry even when creation fails before admission.
        // No foreground can still borrow a session whose last owner is dropping.
        let registry = self.dispatcher.registry();
        if let Ok(mut commands) = registry.try_write() {
            *commands = command_api::CommandRegistry::new();
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let registry = registry.clone();
            runtime.spawn(async move {
                *registry.write().await = command_api::CommandRegistry::new();
            });
        };
    }
}

struct Activity {
    active: bool,
    waiting: usize,
    idle_since: Instant,
}
impl Activity {
    fn new() -> Self {
        Self {
            active: false,
            waiting: 0,
            idle_since: Instant::now(),
        }
    }
    fn wait(&mut self) {
        if self.waiting == 0 {
            self.idle_since = Instant::now();
        }
        self.waiting += 1;
    }
    fn resume(&mut self) {
        self.waiting = self.waiting.saturating_sub(1);
    }
    fn finish(&mut self) {
        self.active = false;
        self.waiting = 0;
        self.idle_since = Instant::now();
    }
    fn expired(&self, now: Instant, ttl: Duration) -> bool {
        (!self.active || self.waiting > 0) && now.duration_since(self.idle_since) >= ttl
    }
}
struct ReplaySink {
    side_id: String,
    owner: Arc<dyn ClientEventSink>,
    child: Option<Arc<dyn ClientEventSink>>,
    events: Mutex<Vec<ClientEvent>>,
    order: Mutex<()>,
    activity: std::sync::Mutex<Activity>,
}
#[async_trait]
impl ClientEventSink for ReplaySink {
    async fn emit(&self, event: ClientEvent) {
        let _order = self.order.lock().await;
        {
            let mut activity = self
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &event {
                ClientEvent::AskUserQuestion { .. } => activity.wait(),
                ClientEvent::AskUserQuestionResolved { .. }
                | ClientEvent::PermissionRequestResolved { .. } => activity.resume(),
                _ => {}
            }
        }
        self.events.lock().await.push(event.clone());
        if let Some(child) = &self.child {
            child.emit(event).await;
        } else if let Ok(event_json) = serde_json::to_string(&event) {
            self.owner
                .emit(ClientEvent::SideChatEvent {
                    side_id: self.side_id.clone(),
                    event_json,
                })
                .await;
        }
    }
}
struct SidePermissionSink {
    replay: Arc<ReplaySink>,
    child: Option<Arc<dyn PermissionRequestSink>>,
}
#[async_trait]
impl PermissionRequestSink for SidePermissionSink {
    async fn emit_request(&self, request: PermissionRequest) {
        let _order = self.replay.order.lock().await;
        self.replay
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wait();
        self.replay
            .events
            .lock()
            .await
            .push(ClientEvent::SideChatPermission {
                side_id: self.replay.side_id.clone(),
                request: request.clone(),
            });
        if let Some(child) = &self.child {
            child.emit_request(request).await;
        } else {
            self.replay
                .owner
                .emit(ClientEvent::SideChatPermission {
                    side_id: self.replay.side_id.clone(),
                    request,
                })
                .await;
        }
    }
}
struct RunningTurn {
    id: Option<u64>,
    cancel: CancellationToken,
    done: Arc<Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}
struct TurnCompletionGuard {
    finished: Arc<std::sync::atomic::AtomicBool>,
    done: Arc<Notify>,
    cleanup_failed: Arc<std::sync::atomic::AtomicBool>,
    complete: bool,
}
impl Drop for TurnCompletionGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.cleanup_failed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.finished
            .store(true, std::sync::atomic::Ordering::Release);
        self.done.notify_waiters();
    }
}
struct SideState {
    closing: bool,
    submissions: HashMap<String, ()>,
    running: Option<RunningTurn>,
}
struct Side {
    session: SideChatSession,
    replay: Arc<ReplaySink>,
    state: Mutex<SideState>,
}
struct ManagerState {
    closed: bool,
    sides: HashMap<String, Arc<Side>>,
    requests: HashMap<String, Result<String, String>>,
}

/// One manager belongs to one host generation. Request tombstones survive End
/// so stale retries never recreate children.
pub struct SideChatManager {
    parent: Arc<ConversationOrchestrator>,
    factory: Arc<dyn SideChatFactory>,
    owner: Arc<dyn ClientEventSink>,
    state: Mutex<ManagerState>,
    create_gate: Mutex<()>,
    ttl: Duration,
}
impl SideChatManager {
    /// Create the owner for one host generation.
    pub fn new(
        parent: Arc<ConversationOrchestrator>,
        factory: Arc<dyn SideChatFactory>,
        owner: Arc<dyn ClientEventSink>,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            parent,
            factory,
            owner,
            state: Mutex::new(ManagerState {
                closed: false,
                sides: HashMap::new(),
                requests: HashMap::new(),
            }),
            create_gate: Mutex::new(()),
            ttl: Duration::from_secs(30 * 60),
        });
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                let sides: Vec<_> = manager
                    .state
                    .lock()
                    .await
                    .sides
                    .iter()
                    .map(|(id, side)| (id.clone(), side.clone()))
                    .collect();
                for (id, side) in sides {
                    let expired = side
                        .replay
                        .activity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .expired(Instant::now(), manager.ttl);
                    if expired && manager.end(&id).await.is_ok() {
                        manager
                            .owner
                            .emit(ClientEvent::SideChatEnded { side_id: id })
                            .await;
                    }
                }
                if manager.state.lock().await.closed {
                    break;
                }
            }
        });
        manager
    }

    /// Creation is manager-owned even when the requesting UI future is dropped.
    pub async fn create(
        self: &Arc<Self>,
        request_id: String,
        initial_input: Option<ClientTextDto>,
        child_sink: Option<Arc<dyn ClientEventSink>>,
        permission_sink: Option<Arc<dyn PermissionRequestSink>>,
    ) -> Result<String, String> {
        let this = self.clone();
        tokio::spawn(async move {
            this.create_owned(request_id, initial_input, child_sink, permission_sink)
                .await
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn create_owned(
        self: &Arc<Self>,
        request_id: String,
        initial_input: Option<ClientTextDto>,
        child_sink: Option<Arc<dyn ClientEventSink>>,
        permission_sink: Option<Arc<dyn PermissionRequestSink>>,
    ) -> Result<String, String> {
        let _creation = self.create_gate.lock().await;
        {
            let state = self.state.lock().await;
            if state.closed {
                return Err("side chat host is closed".into());
            }
            if let Some(result) = state.requests.get(&request_id) {
                let result = match result {
                    Ok(id) if state.sides.contains_key(id) => Ok(id.clone()),
                    Ok(_) => Err("side chat has ended".into()),
                    Err(error) => Err(error.clone()),
                };
                drop(state);
                if let Ok(side_id) = &result {
                    self.expire_idle(side_id).await?;
                    self.owner
                        .emit(ClientEvent::SideChatCreated {
                            request_id,
                            side_id: side_id.clone(),
                        })
                        .await;
                }
                return result;
            }
        }
        let side_id = uuid::Uuid::new_v4().to_string();
        let replay = Arc::new(ReplaySink {
            side_id: side_id.clone(),
            owner: self.owner.clone(),
            child: child_sink,
            events: Mutex::new(Vec::new()),
            order: Mutex::new(()),
            activity: std::sync::Mutex::new(Activity::new()),
        });
        let permission = Arc::new(SidePermissionSink {
            replay: replay.clone(),
            child: permission_sink,
        });
        let result = self
            .factory
            .build(
                self.parent.side_chat_parent_snapshot().await,
                replay.clone(),
                permission,
            )
            .await;
        let session = match result {
            Ok(session) => session,
            Err(error) => {
                self.state
                    .lock()
                    .await
                    .requests
                    .insert(request_id, Err(error.clone()));
                return Err(error);
            }
        };
        if let Some(text) = initial_input
            .as_ref()
            .filter(|text| text_starts_with_slash(text))
        {
            let raw = text.clone().into_projection();
            if let Err(error) = validate_slash(&session.dispatcher, &raw).await {
                self.state
                    .lock()
                    .await
                    .requests
                    .insert(request_id, Err(error.clone()));
                return Err(error);
            }
        }
        let side = Arc::new(Side {
            session,
            replay,
            state: Mutex::new(SideState {
                closing: false,
                submissions: HashMap::new(),
                running: None,
            }),
        });
        self.state.lock().await.sides.insert(side_id.clone(), side);
        self.state
            .lock()
            .await
            .requests
            .insert(request_id.clone(), Ok(side_id.clone()));
        self.owner
            .emit(ClientEvent::SideChatCreated {
                request_id: request_id.clone(),
                side_id: side_id.clone(),
            })
            .await;
        if let Some(text) = initial_input.filter(text_has_non_whitespace) {
            self.submit(
                &side_id,
                format!("create:{request_id}"),
                ClientCommand::SendPrompt {
                    text,
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(1),
                    visualization_context: None,
                },
            )
            .await?;
        }
        Ok(side_id)
    }
    async fn side(&self, id: &str) -> Result<Arc<Side>, String> {
        self.state
            .lock()
            .await
            .sides
            .get(id)
            .cloned()
            .ok_or_else(|| "side chat is unavailable".into())
    }
    async fn expire_idle(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let side = self.side(id).await?;
        if side.state.lock().await.closing {
            return Ok(());
        }
        let expired = side
            .replay
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expired(Instant::now(), self.ttl);
        if expired {
            if self.end(id).await.is_ok() {
                self.owner
                    .emit(ClientEvent::SideChatEnded {
                        side_id: id.to_string(),
                    })
                    .await;
            }
            return Err("side chat has expired".into());
        }
        Ok(())
    }
    /// List retained identities after applying idle expiration.
    pub async fn list(self: &Arc<Self>) -> Vec<String> {
        let ids = {
            let _creation = self.create_gate.lock().await;
            self.state
                .lock()
                .await
                .sides
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        for id in ids {
            let _ = self.expire_idle(&id).await;
        }
        let mut ids = self
            .state
            .lock()
            .await
            .sides
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }
    /// Read authoritative admission state after applying idle expiration.
    pub async fn info(self: &Arc<Self>, id: &str) -> Result<SideChatInfoDto, String> {
        self.expire_idle(id).await?;
        let side = self.side(id).await?;
        let status = if side.state.lock().await.closing {
            "closing"
        } else {
            "open"
        };
        Ok(SideChatInfoDto {
            side_id: id.to_string(),
            status: status.into(),
        })
    }
    /// Clone the new-message replay without inheriting the parent timeline.
    pub async fn snapshot(self: &Arc<Self>, id: &str) -> Result<Vec<ClientEvent>, String> {
        self.expire_idle(id).await?;
        let side = self.side(id).await?;
        let _order = side.replay.order.lock().await;
        let events = side.replay.events.lock().await.clone();
        Ok(events)
    }
    /// Preserve snapshot/live event order through delivery to the owner.
    pub async fn emit_snapshot(self: &Arc<Self>, id: &str) -> Result<(), String> {
        self.expire_idle(id).await?;
        let side = self.side(id).await?;
        let _order = side.replay.order.lock().await;
        let events_json = side
            .replay
            .events
            .lock()
            .await
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        self.owner
            .emit(ClientEvent::SideChatSnapshot {
                side_id: id.to_string(),
                events_json,
            })
            .await;
        Ok(())
    }

    /// Admit one ordinary child command, deduplicated by request ID.
    pub async fn submit(
        self: &Arc<Self>,
        id: &str,
        request_id: String,
        command: ClientCommand,
    ) -> Result<(), String> {
        if self.state.lock().await.closed {
            return Err("side chat host is closed".into());
        }
        self.expire_idle(id).await?;
        let side = self.side(id).await?;
        let mut state = side.state.lock().await;
        if side
            .session
            .cleanup_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            state.closing = true;
        }
        if state.closing {
            return Err("side chat is closing".into());
        }
        if state.submissions.contains_key(&request_id) {
            drop(state);
            self.owner
                .emit(ClientEvent::SideChatSubmitted {
                    side_id: id.to_string(),
                    request_id,
                })
                .await;
            return Ok(());
        }
        if let ClientCommand::Cancel { turn_id } = command {
            let wait = state
                .running
                .as_ref()
                .filter(|running| turn_id.is_none() || running.id == turn_id)
                .map(|running| {
                    running.cancel.cancel();
                    (running.done.clone(), running.finished.clone())
                });
            state.submissions.insert(request_id.clone(), ());
            drop(state);
            if let Some((done, finished)) = wait {
                side.session.permission_gate.drain().await;
                side.session.question_broker.drain().await;
                while !finished.load(std::sync::atomic::Ordering::Acquire) {
                    let notified = done.notified();
                    if finished.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    notified.await;
                }
            }
            self.owner
                .emit(ClientEvent::SideChatSubmitted {
                    side_id: id.to_string(),
                    request_id,
                })
                .await;
            return Ok(());
        }
        let raw = match &command {
            ClientCommand::RunSlashCommand { raw, .. } => Some(raw.clone().into_projection()),
            ClientCommand::SendPrompt { text, .. } if text_starts_with_slash(text) => {
                Some(text.clone().into_projection())
            }
            _ => None,
        };
        let native_prompt_slash = match raw {
            Some(raw) => validate_slash(&side.session.dispatcher, &raw).await?,
            None => false,
        };
        let command = match command {
            ClientCommand::SendPrompt {
                text,
                turn_id,
                images,
                ..
            } if text_starts_with_slash(&text) => ClientCommand::RunSlashCommand {
                raw: text,
                images,
                turn_id,
            },
            command => command,
        };
        let turn = matches!(
            command,
            ClientCommand::SendPrompt { .. }
                | ClientCommand::RunSlashCommand { .. }
                | ClientCommand::ForceCompact
        );
        if turn && state.running.is_some() {
            return Err("side chat is busy".into());
        }
        if !allowed_command(&command) {
            return Err("command is unavailable in side chat".into());
        }
        if !turn {
            drop(state);
            dispatch(&side, id, command, CancellationToken::new(), false).await?;
            side.state
                .lock()
                .await
                .submissions
                .insert(request_id.clone(), ());
            self.owner
                .emit(ClientEvent::SideChatSubmitted {
                    side_id: id.to_string(),
                    request_id,
                })
                .await;
            return Ok(());
        }
        let cancel = CancellationToken::new();
        let done = Arc::new(Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if turn {
            let turn_id = match &command {
                ClientCommand::SendPrompt { turn_id, .. }
                | ClientCommand::RunSlashCommand { turn_id, .. } => *turn_id,
                _ => None,
            };
            state.running = Some(RunningTurn {
                id: turn_id,
                cancel: cancel.clone(),
                done: done.clone(),
                finished: finished.clone(),
            });
            side.replay
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active = true;
        }
        state.submissions.insert(request_id.clone(), ());
        self.owner
            .emit(ClientEvent::SideChatSubmitted {
                side_id: id.to_string(),
                request_id,
            })
            .await;
        drop(state);
        if turn {
            let side_id = id.to_string();
            tokio::spawn(async move {
                let mut completion = TurnCompletionGuard {
                    finished,
                    done,
                    cleanup_failed: side.session.cleanup_failed.clone(),
                    complete: false,
                };
                let replay_start = side.replay.events.lock().await.len();
                let terminal_cancel = cancel.clone();
                let slash_turn_id = match &command {
                    ClientCommand::RunSlashCommand { turn_id, .. } => Some(*turn_id),
                    _ => None,
                };
                if let Err(error) =
                    dispatch(&side, &side_id, command, cancel, native_prompt_slash).await
                {
                    if let Some(turn_id) = slash_turn_id {
                        side.replay
                            .emit(ClientEvent::SlashCommandResult {
                                turn_id,
                                display: error.into(),
                                is_error: true,
                            })
                            .await;
                    } else {
                        side.replay
                            .emit(ClientEvent::Error {
                                kind: client::protocol::events::ErrorKindDto::Internal,
                                message: error.into(),
                            })
                            .await;
                    }
                }
                let missing_terminal = {
                    let replay = side.replay.events.lock().await;
                    let turn = &replay[replay_start..];
                    turn.iter()
                        .any(|event| matches!(event, ClientEvent::TurnStarted { .. }))
                        && !turn
                            .iter()
                            .any(|event| matches!(event, ClientEvent::TurnEnded { .. }))
                };
                if missing_terminal {
                    side.replay
                        .emit(ClientEvent::TurnEnded {
                            outcome: if terminal_cancel.is_cancelled() {
                                client::protocol::events::TurnOutcomeDto::Cancelled
                            } else {
                                client::protocol::events::TurnOutcomeDto::EndTurn
                            },
                            stop_reason: None,
                            cost: client::adapter::lowering::lower_cost_snapshot(
                                &side.session.orchestrator.snapshot_cost_real().await,
                            ),
                        })
                        .await;
                }
                side.session.permission_gate.drain().await;
                side.session.question_broker.drain().await;
                let mut state = side.state.lock().await;
                state.running = None;
                if side
                    .session
                    .cleanup_failed
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    state.closing = true;
                }
                drop(state);
                side.replay
                    .activity
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .finish();
                completion.complete = true;
            });
            Ok(())
        } else {
            dispatch(&side, id, command, cancel, false).await
        }
    }
    /// End is owned by the manager and awaits foreground cleanup. Closing
    /// records remain retained if cleanup cannot complete.
    pub async fn end(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let this = self.clone();
        let id = id.to_string();
        tokio::spawn(async move { this.end_owned(&id).await })
            .await
            .map_err(|e| e.to_string())?
    }
    async fn end_owned(&self, id: &str) -> Result<(), String> {
        let side = self.side(id).await?;
        let mut state = side.state.lock().await;
        state.closing = true;
        let wait = state.running.as_ref().map(|running| {
            running.cancel.cancel();
            (running.done.clone(), running.finished.clone())
        });
        drop(state);
        side.session.permission_gate.drain().await;
        side.session.question_broker.drain().await;
        if let Some((done, finished)) = wait {
            while !finished.load(std::sync::atomic::Ordering::Acquire) {
                let notified = done.notified();
                if finished.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }
        if side
            .session
            .cleanup_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("side foreground cleanup failed; conversation remains closing".into());
        }
        // Builtin handlers hold the engine, whose Skill loader/listing holds this
        // registry. Break that backreference only after foreground cleanup succeeds.
        *side.session.dispatcher.registry().write().await = command_api::CommandRegistry::new();
        if let Some(child) = &side.replay.child {
            child
                .emit(ClientEvent::SideChatEnded {
                    side_id: id.to_string(),
                })
                .await;
        }
        self.state.lock().await.sides.remove(id);
        Ok(())
    }
    /// Close all admissions and drain every child, collecting cleanup failures.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), String> {
        let this = self.clone();
        tokio::spawn(async move { this.shutdown_owned().await })
            .await
            .map_err(|error| error.to_string())?
    }
    async fn shutdown_owned(self: &Arc<Self>) -> Result<(), String> {
        let _creation = self.create_gate.lock().await;
        let sides = {
            let mut state = self.state.lock().await;
            state.closed = true;
            state
                .sides
                .iter()
                .map(|(id, side)| (id.clone(), side.clone()))
                .collect::<Vec<_>>()
        };
        // Close every admission and cancel every foreground owner before any
        // potentially failing drain. One failed child cannot strand siblings.
        for (_, side) in &sides {
            let mut state = side.state.lock().await;
            state.closing = true;
            if let Some(running) = &state.running {
                running.cancel.cancel();
            }
        }
        let mut errors = Vec::new();
        for (id, _) in sides {
            if let Err(error) = self.end_owned(&id).await {
                errors.push(format!("{id}: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
impl Drop for SideChatManager {
    fn drop(&mut self) {
        if let Ok(state) = self.state.try_lock() {
            for side in state.sides.values() {
                if let Ok(child) = side.state.try_lock() {
                    if let Some(running) = &child.running {
                        running.cancel.cancel();
                    }
                }
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let side = side.clone();
                    runtime.spawn(async move {
                        side.session.permission_gate.drain().await;
                        side.session.question_broker.drain().await;
                        let wait = side
                            .state
                            .lock()
                            .await
                            .running
                            .as_ref()
                            .map(|running| (running.done.clone(), running.finished.clone()));
                        if let Some((done, finished)) = wait {
                            while !finished.load(std::sync::atomic::Ordering::Acquire) {
                                let notified = done.notified();
                                if finished.load(std::sync::atomic::Ordering::Acquire) {
                                    break;
                                }
                                notified.await;
                            }
                        }
                        if !side
                            .session
                            .cleanup_failed
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            *side.session.dispatcher.registry().write().await =
                                command_api::CommandRegistry::new();
                        }
                    });
                }
            }
        }
    }
}
fn text_starts_with_slash(text: &ClientTextDto) -> bool {
    text.code_units.first().copied() == Some(b'/' as u16)
}

fn text_has_non_whitespace(text: &ClientTextDto) -> bool {
    text.code_units
        .iter()
        .copied()
        .any(|unit| !is_js_whitespace_unit(unit))
}

fn is_js_whitespace_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn text_from_source(source: &Utf16JsonProjection) -> ClientTextDto {
    ClientTextDto::from_projection(source).expect("side-chat source text is a root string")
}

async fn validate_slash(
    dispatcher: &RegistrySlashDispatcher,
    raw: &Utf16JsonProjection,
) -> Result<bool, String> {
    if raw
        .string_units("")
        .is_none_or(|units| units.first().copied() != Some(b'/' as u16))
    {
        return Err("expected slash command".into());
    }
    let parsed =
        command_api::parse_slash_command(raw).ok_or_else(|| "invalid slash command".to_string())?;
    let registry = dispatcher.registry();
    let registry = registry.read().await;
    let descriptor = registry
        .resolve(&parsed.name)
        .ok_or_else(|| "command is unavailable in side chat".to_string())?;
    if registry.execution_allowed(descriptor) {
        Ok(descriptor.native_type == Some(command_api::model::NativeCommandType::Prompt))
    } else {
        Err("command is unavailable in side chat".into())
    }
}

async fn emit_cancelled_slash_result(side: &Side, turn_id: Option<u64>) {
    side.replay
        .emit(ClientEvent::SlashCommandResult {
            turn_id,
            display: "Slash command was cancelled before entering the model.".into(),
            is_error: true,
        })
        .await;
}

fn allowed_command(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::SendPrompt { .. }
            | ClientCommand::RunSlashCommand { .. }
            | ClientCommand::ForceCompact
            | ClientCommand::ApprovePermission { .. }
            | ClientCommand::DenyPermission { .. }
            | ClientCommand::AnswerAskUserQuestion { .. }
            | ClientCommand::CancelAskUserQuestion { .. }
            | ClientCommand::SetPermissionMode { .. }
            | ClientCommand::SetModel { .. }
            | ClientCommand::ListModels
            | ClientCommand::GetConversationControls
            | ClientCommand::SetReasoningSelection { .. }
            | ClientCommand::RefreshListings { .. }
    )
}
async fn dispatch(
    side: &Side,
    id: &str,
    command: ClientCommand,
    cancel: CancellationToken,
    native_prompt_slash: bool,
) -> Result<(), String> {
    let orch = &side.session.orchestrator;
    match command {
        // Native composers use this typed command for /compact. Keep it in
        // the child's foreground slot so Stop/End own cancellation, and keep
        // the terminal error contract consumed by the native progress UI.
        ClientCommand::ForceCompact => orch
            .force_compact_with_cancel(cancel)
            .await
            .map(|_| ())
            .map_err(|error| format!("force_compact failed: {error}")),
        ClientCommand::SendPrompt {
            text,
            images,
            turn_id,
            ..
        } => {
            let prompt = text.clone().into_projection();
            let replay_images = images
                .iter()
                .map(|image| client::protocol::message::MessageImageDto {
                    media_type: image.media_type.clone(),
                    url: format!("data:{};base64,{}", image.media_type, image.base64),
                })
                .collect();
            let images = images
                .into_iter()
                .map(|image| lingxi_core::types::ImageSource::Base64 {
                    media_type: image.media_type,
                    data: image.base64,
                })
                .collect();
            {
                let _order = side.replay.order.lock().await;
                side.replay
                    .events
                    .lock()
                    .await
                    .push(ClientEvent::MessageComplete {
                        stop_reason: None,
                        message: Some(client::protocol::message::MessageDto {
                            role: "user".into(),
                            blocks: vec![client::protocol::message::MessageBlockDto::Text {
                                text: text.clone(),
                            }],
                            images: replay_images,
                            loop_wakeup: None,
                            visualization_context: None,
                            sent_at_ms: Some(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as i64,
                            ),
                        }),
                    });
            }
            side.replay.emit(ClientEvent::TurnStarted { turn_id }).await;
            let owner = side.session.permission_gate.begin_main_turn(None, turn_id);
            let result = orch
                .run_turn_streaming_with_cancel_image_sources_and_message_id(
                    prompt, images, cancel, None,
                )
                .await;
            side.session.permission_gate.end_main_turn(owner);
            result.map(|_| ()).map_err(|error| error.to_string())
        }
        ClientCommand::RunSlashCommand {
            raw,
            images,
            turn_id,
        } => {
            if cancel.is_cancelled() {
                emit_cancelled_slash_result(side, turn_id).await;
                return Ok(());
            }
            let replay_images: Vec<client::protocol::message::MessageImageDto> = images
                .iter()
                .map(|image| client::protocol::message::MessageImageDto {
                    media_type: image.media_type.clone(),
                    url: format!("data:{};base64,{}", image.media_type, image.base64),
                })
                .collect();
            let image_sources = images
                .into_iter()
                .map(|image| lingxi_core::types::ImageSource::Base64 {
                    media_type: image.media_type,
                    data: image.base64,
                })
                .collect::<Vec<_>>();
            let raw_source = raw.clone().into_projection();
            let dispatch_result = side
                .session
                .dispatcher
                .dispatch_with_cancel(raw_source.clone(), id.to_string(), cancel.clone())
                .await;
            if native_prompt_slash && cancel.is_cancelled() {
                emit_cancelled_slash_result(side, turn_id).await;
                return Ok(());
            }
            match dispatch_result {
                SlashDispatchResult::RunAsTurn { prompt } => {
                    if cancel.is_cancelled() {
                        emit_cancelled_slash_result(side, turn_id).await;
                        return Ok(());
                    }
                    {
                        let _order = side.replay.order.lock().await;
                        side.replay
                            .events
                            .lock()
                            .await
                            .push(ClientEvent::MessageComplete {
                                stop_reason: None,
                                message: Some(client::protocol::message::MessageDto {
                                    role: "user".into(),
                                    blocks: vec![
                                        client::protocol::message::MessageBlockDto::Text {
                                            text: raw.clone(),
                                        },
                                    ],
                                    images: replay_images.clone(),
                                    loop_wakeup: None,
                                    visualization_context: None,
                                    sent_at_ms: None,
                                }),
                            });
                    }
                    side.replay.emit(ClientEvent::TurnStarted { turn_id }).await;
                    let owner = side.session.permission_gate.begin_main_turn(None, turn_id);
                    let result = orch
                        .run_turn_streaming_with_cancel_image_sources_and_message_id(
                            prompt,
                            image_sources,
                            cancel,
                            None,
                        )
                        .await;
                    side.session.permission_gate.end_main_turn(owner);
                    result.map(|_| ()).map_err(|e| e.to_string())
                }
                SlashDispatchResult::RunSkillAsTurn {
                    command,
                    body,
                    hook_messages,
                } => {
                    if cancel.is_cancelled() {
                        emit_cancelled_slash_result(side, turn_id).await;
                        return Ok(());
                    }
                    {
                        let _order = side.replay.order.lock().await;
                        side.replay
                            .events
                            .lock()
                            .await
                            .push(ClientEvent::MessageComplete {
                                stop_reason: None,
                                message: Some(client::protocol::message::MessageDto {
                                    role: "user".into(),
                                    blocks: vec![
                                        client::protocol::message::MessageBlockDto::Text {
                                            text: raw.clone(),
                                        },
                                    ],
                                    images: replay_images.clone(),
                                    loop_wakeup: None,
                                    visualization_context: None,
                                    sent_at_ms: None,
                                }),
                            });
                    }
                    side.replay.emit(ClientEvent::TurnStarted { turn_id }).await;
                    let mut inputs =
                        orchestrator::skill_prompt_inputs(command, body, hook_messages);
                    if let Some(input) = inputs.iter_mut().find(|input| !input.is_meta) {
                        input.images = image_sources;
                    }
                    let owner = side.session.permission_gate.begin_main_turn(None, turn_id);
                    let result = orch.run_queued_prompt_batch(inputs, cancel).await;
                    side.session.permission_gate.end_main_turn(owner);
                    result.map(|_| ()).map_err(|error| error.to_string())
                }
                SlashDispatchResult::Handled { display } => {
                    if !replay_images.is_empty() {
                        let _order = side.replay.order.lock().await;
                        side.replay
                            .events
                            .lock()
                            .await
                            .push(ClientEvent::MessageComplete {
                                stop_reason: None,
                                message: Some(client::protocol::message::MessageDto {
                                    role: "user".into(),
                                    blocks: vec![
                                        client::protocol::message::MessageBlockDto::Text {
                                            text: raw.clone(),
                                        },
                                    ],
                                    images: replay_images,
                                    loop_wakeup: None,
                                    visualization_context: None,
                                    sent_at_ms: None,
                                }),
                            });
                    }
                    side.replay
                        .emit(ClientEvent::SlashCommandResult {
                            turn_id,
                            display: text_from_source(&display),
                            is_error: false,
                        })
                        .await;
                    Ok(())
                }
                SlashDispatchResult::Failed { diagnostic } => {
                    let display = diagnostic
                        .string_units("/message")
                        .or_else(|| diagnostic.string_units(""))
                        .map(ClientTextDto::from_code_units)
                        .unwrap_or_else(|| "command settings failed".into());
                    side.replay
                        .emit(ClientEvent::SlashCommandResult {
                            turn_id,
                            display,
                            is_error: true,
                        })
                        .await;
                    Ok(())
                }
                SlashDispatchResult::Unknown { display, .. } => {
                    side.replay
                        .emit(ClientEvent::SlashCommandResult {
                            turn_id,
                            display: text_from_source(&display),
                            is_error: true,
                        })
                        .await;
                    Ok(())
                }
                SlashDispatchResult::ParseFailure { .. } => {
                    side.replay
                        .emit(ClientEvent::SlashCommandResult {
                            turn_id,
                            display: "Commands are in the form `/command [args]`".into(),
                            is_error: false,
                        })
                        .await;
                    Ok(())
                }
                SlashDispatchResult::NotASlashCommand => {
                    let mut code_units = "not a slash command: ".encode_utf16().collect::<Vec<_>>();
                    code_units.extend(raw_source.string_units("").unwrap_or_default());
                    side.replay
                        .emit(ClientEvent::SlashCommandResult {
                            turn_id,
                            display: ClientTextDto::from_code_units(code_units),
                            is_error: true,
                        })
                        .await;
                    Ok(())
                }
            }
        }
        ClientCommand::ApprovePermission {
            request_id,
            response,
        } => {
            if side
                .session
                .permission_gate
                .resolve(request_id, response, "")
                .await
            {
                Ok(())
            } else {
                Err("permission request has expired".into())
            }
        }
        ClientCommand::DenyPermission { request_id } => {
            side.session
                .permission_gate
                .resolve(request_id, PermissionResponseDto::Deny, "")
                .await;
            Ok(())
        }
        ClientCommand::AnswerAskUserQuestion {
            request_id,
            answers,
        } => {
            let answers = answers
                .pairs
                .into_iter()
                .map(|pair| {
                    (
                        pair.question.into_projection(),
                        pair.answer.into_projection(),
                    )
                })
                .collect();
            if side
                .session
                .question_broker
                .resolve(request_id, answers)
                .await
            {
                Ok(())
            } else {
                Err("question has expired".into())
            }
        }
        ClientCommand::CancelAskUserQuestion { request_id } => {
            side.session.question_broker.cancel(request_id).await;
            Ok(())
        }
        ClientCommand::SetPermissionMode { mode } => {
            orch.set_permission_mode(&mode).await?;
            side.replay
                .emit(ClientEvent::PermissionModeChanged { mode })
                .await;
            Ok(())
        }
        ClientCommand::SetModel { model } => {
            let model_selection = model.into_projection();
            let listings = orch.list_model_listings().await;
            let (source, route_hint) = split_model_selection_source(model_selection, &listings);
            orch.select_model(lingxi_core::host::ModelSelectionRequest::Select {
                source,
                route_hint,
                origin: lingxi_core::host::ModelSelectionOrigin::Sdk,
                validation: lingxi_core::host::ModelValidationMode::GzrAbortAfterFiveSeconds,
            })
            .await
            .map_err(|e| e.to_string())?;
            let snapshot = orch.get_status_snapshot().await;
            let model = lingxi_core::host::qualified_model_ref(
                &snapshot.model,
                snapshot.model_profile.as_deref(),
            );
            side.replay
                .emit(ClientEvent::ModelChanged {
                    model,
                    model_source: lower_model_source(snapshot.model_source.as_ref())
                        .map_err(str::to_owned)?,
                })
                .await;
            if let Some(controls) = orch.conversation_controls().await {
                side.replay
                    .emit(ClientEvent::ConversationControlsChanged {
                        controls: client::adapter::controls::lower_conversation_controls(
                            controls,
                            orch.native_proactivity_snapshot(),
                        ),
                    })
                    .await;
            }
            Ok(())
        }
        ClientCommand::ListModels => {
            let snapshot = orch.get_status_snapshot().await;
            side.replay
                .emit(ClientEvent::ModelList {
                    models: orch.list_available_models().await,
                    current: lingxi_core::host::qualified_model_ref(
                        &snapshot.model,
                        snapshot.model_profile.as_deref(),
                    ),
                    current_model_source: lower_model_source(snapshot.model_source.as_ref())
                        .map_err(str::to_owned)?,
                    details: orch
                        .list_model_listings()
                        .await
                        .iter()
                        .map(client::adapter::lowering::lower_model_details)
                        .collect(),
                })
                .await;
            Ok(())
        }
        ClientCommand::GetConversationControls => {
            if let Some(controls) = orch.conversation_controls().await {
                side.replay
                    .emit(ClientEvent::ConversationControlsChanged {
                        controls: client::adapter::controls::lower_conversation_controls(
                            controls,
                            orch.native_proactivity_snapshot(),
                        ),
                    })
                    .await;
            }
            Ok(())
        }
        ClientCommand::RefreshListings { which } => {
            if which
                .iter()
                .any(|kind| matches!(kind, client::protocol::commands::ListingKindDto::Models))
            {
                let snapshot = orch.get_status_snapshot().await;
                side.replay
                    .emit(ClientEvent::ModelList {
                        models: orch.list_available_models().await,
                        current: lingxi_core::host::qualified_model_ref(
                            &snapshot.model,
                            snapshot.model_profile.as_deref(),
                        ),
                        current_model_source: lower_model_source(snapshot.model_source.as_ref())
                            .map_err(str::to_owned)?,
                        details: orch
                            .list_model_listings()
                            .await
                            .iter()
                            .map(client::adapter::lowering::lower_model_details)
                            .collect(),
                    })
                    .await;
            }
            if which.iter().any(|kind| {
                matches!(
                    kind,
                    client::protocol::commands::ListingKindDto::SlashCommands
                )
            }) {
                let registry = side.session.dispatcher.registry();
                let registry = registry.read().await;
                let commands = registry
                    .palette_commands()
                    .into_iter()
                    .map(|command| client::protocol::listings::SlashCommandDto {
                        name: text_from_source(&command.name),
                        description: text_from_source(&command.description),
                        source: format!("{:?}", command.source).to_lowercase(),
                        aliases: command.aliases.iter().map(text_from_source).collect(),
                        argument_hint: command.argument_hint.as_ref().map(text_from_source),
                        argument_names: if matches!(
                            command.kind,
                            command_api::SlashCommandKind::Builtin { .. }
                        ) {
                            Vec::new()
                        } else {
                            command
                                .argument_names
                                .iter()
                                .map(text_from_source)
                                .collect()
                        },
                        menu_description: command.menu_description,
                        hidden: false,
                    })
                    .collect();
                side.replay
                    .emit(ClientEvent::SlashCommandCatalog { commands })
                    .await;
            }
            Ok(())
        }
        ClientCommand::SetReasoningSelection { selection } => {
            let snapshot = orch.side_chat_parent_snapshot().await;
            orch.set_reasoning_selection_for_model(
                &snapshot.config.model,
                snapshot.model_profile.as_deref(),
                client::adapter::controls::decode_reasoning_selection(selection),
            );
            if let Some(controls) = orch.conversation_controls().await {
                side.replay
                    .emit(ClientEvent::ConversationControlsChanged {
                        controls: client::adapter::controls::lower_conversation_controls(
                            controls,
                            orch.native_proactivity_snapshot(),
                        ),
                    })
                    .await;
            }
            Ok(())
        }
        _ => Err("command is unavailable in side chat".into()),
    }
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    #[derive(Default)]
    struct Sink(Mutex<Vec<ClientEvent>>);
    #[async_trait]
    impl ClientEventSink for Sink {
        async fn emit(&self, event: ClientEvent) {
            self.0.lock().await.push(event);
        }
    }
    fn parent() -> Arc<ConversationOrchestrator> {
        ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            orchestrator::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        ))
    }
    struct Factory;
    #[async_trait]
    impl SideChatFactory for Factory {
        async fn build(
            &self,
            snapshot: SideChatParentSnapshot,
            sink: Arc<dyn ClientEventSink>,
            permission: Arc<dyn PermissionRequestSink>,
        ) -> Result<SideChatSession, String> {
            let gate =
                Arc::new(AdapterPermissionGate::new(permission).with_event_sink(sink.clone()));
            let question = Arc::new(AskUserQuestionBroker::new(sink.clone()));
            let commands = Arc::new(tokio::sync::RwLock::new(command_api::CommandRegistry::new()));
            let listing = composition::skill_listing(
                commands.clone(),
                tool_api::read_file_state::new_read_file_state_map(),
                snapshot.cwd.clone(),
            );
            let orch = ConversationOrchestrator::into_shared(
                ConversationOrchestrator::new_side_chat(
                    snapshot.config.clone(),
                    Arc::new(MockApiClient::new(vec![])),
                    Arc::new(
                        orchestrator::test_support::MockStreamingApiClient::with_turns(vec![]),
                    ),
                    Arc::new(tool_api::ToolRegistry::new()),
                    noop_hook_executor(),
                    gate.clone(),
                    Arc::new(client::adapter::AdapterOutputStream::new(sink)),
                    Arc::new(StaticMemoryProvider::empty()),
                    snapshot.cwd.clone(),
                )
                .with_skill_listing(listing),
            );
            orch.seed_side_chat(&snapshot).await?;
            commands.write().await.register_builtin_handler(Arc::new(
                command_api::builtins::CompactHandler::new(
                    orch.clone() as Arc<dyn OrchestratorHandle>
                ),
            ));
            commands.write().await.register_builtin_handler(Arc::new(
                command_api::builtins::ModelHandler::new(
                    orch.clone() as Arc<dyn OrchestratorHandle>
                ),
            ));
            Ok(SideChatSession {
                orchestrator: orch,
                dispatcher: Arc::new(RegistrySlashDispatcher::new(commands)),
                permission_gate: gate,
                question_broker: question,
                cleanup_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
        }
    }

    struct TestPromptHandler {
        entered: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Option<Arc<Notify>>,
    }

    #[async_trait::async_trait]
    impl command_api::model::BuiltinCommandHandler for TestPromptHandler {
        async fn handle(
            &self,
            _args: &command_api::parser::ParsedSlashCommand,
        ) -> command_api::model::CommandResult {
            if let Some(entered) = self.entered.lock().await.take() {
                let _ = entered.send(());
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            command_api::model::CommandResult::Done {
                display: Some("test prompt handler completed".into()),
            }
        }

        fn name(&self) -> &str {
            "review"
        }

        fn description(&self) -> &str {
            "test prompt command"
        }

        fn native_type(&self) -> Option<command_api::model::NativeCommandType> {
            Some(command_api::model::NativeCommandType::Prompt)
        }
    }

    #[tokio::test]
    async fn end_and_expiry_release_engine_and_replay_but_failed_cleanup_retains_them() {
        let manager = SideChatManager::new(parent(), Arc::new(Factory), Arc::new(Sink::default()));
        for (request, expire, fail) in [
            ("ended", false, false),
            ("expired", true, false),
            ("failed", false, true),
        ] {
            let id = manager
                .create(request.into(), None, None, None)
                .await
                .unwrap();
            let side = manager.side(&id).await.unwrap();
            let engine = Arc::downgrade(&side.session.orchestrator);
            let replay = Arc::downgrade(&side.replay);
            side.session
                .cleanup_failed
                .store(fail, std::sync::atomic::Ordering::Release);
            if expire {
                side.replay.activity.lock().unwrap().idle_since =
                    Instant::now() - Duration::from_secs(1801);
            }
            drop(side);
            if expire {
                manager.list().await;
            } else {
                assert_eq!(manager.end(&id).await.is_err(), fail);
            }
            assert_eq!(engine.upgrade().is_some(), fail);
            assert_eq!(replay.upgrade().is_some(), fail);
            if fail {
                let side = manager.side(&id).await.unwrap();
                assert!(side.state.lock().await.closing);
                side.session
                    .cleanup_failed
                    .store(false, std::sync::atomic::Ordering::Release);
                drop(side);
                manager.end(&id).await.unwrap();
                assert!(engine.upgrade().is_none());
                assert!(replay.upgrade().is_none());
            }
        }
    }
    #[tokio::test]
    async fn unadmitted_session_drop_releases_registry_cycle() {
        let parent = parent();
        let replay = Arc::new(Sink::default());
        let permission = Arc::new(SidePermissionSink {
            replay: Arc::new(ReplaySink {
                side_id: "unadmitted".into(),
                owner: replay.clone(),
                child: None,
                events: Mutex::new(Vec::new()),
                order: Mutex::new(()),
                activity: std::sync::Mutex::new(Activity::new()),
            }),
            child: None,
        });
        let session = Factory
            .build(parent.side_chat_parent_snapshot().await, replay, permission)
            .await
            .unwrap();
        let engine = Arc::downgrade(&session.orchestrator);
        drop(session);
        assert!(engine.upgrade().is_none());
    }
    struct GatedStreaming(Arc<Notify>);
    #[async_trait]
    impl orchestrator::StreamingApiClient for GatedStreaming {
        async fn stream(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
            _: Vec<lingxi_core::types::ConversationMessage>,
            _: Vec<llm_runtime::ToolDeclaration>,
            _: &str,
            _: bool,
            _: Option<llm_runtime::RequestDispatchAdmission>,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
            >,
            llm_runtime::LlmError,
        > {
            self.0.notify_one();
            futures::future::pending().await
        }
    }
    struct GatedFactory(Arc<Notify>);
    #[async_trait]
    impl SideChatFactory for GatedFactory {
        async fn build(
            &self,
            snapshot: SideChatParentSnapshot,
            sink: Arc<dyn ClientEventSink>,
            permission: Arc<dyn PermissionRequestSink>,
        ) -> Result<SideChatSession, String> {
            let mut session = Factory
                .build(snapshot.clone(), sink.clone(), permission)
                .await?;
            session.orchestrator =
                ConversationOrchestrator::into_shared(ConversationOrchestrator::new_side_chat(
                    snapshot.config.clone(),
                    Arc::new(MockApiClient::new(vec![])),
                    Arc::new(GatedStreaming(self.0.clone())),
                    Arc::new(tool_api::ToolRegistry::new()),
                    noop_hook_executor(),
                    session.permission_gate.clone(),
                    Arc::new(client::adapter::AdapterOutputStream::new(sink)),
                    Arc::new(StaticMemoryProvider::empty()),
                    snapshot.cwd.clone(),
                ));
            session.orchestrator.seed_side_chat(&snapshot).await?;
            Ok(session)
        }
    }
    struct GatedFallbackApi(Arc<Notify>);
    #[async_trait]
    impl orchestrator::OrchestratorApiClient for GatedFallbackApi {
        async fn messages_create(
            &self,
            _: orchestrator::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            self.0.notify_one();
            futures::future::pending().await
        }
    }
    struct GatedFallbackFactory(Arc<Notify>, bool);
    #[async_trait]
    impl SideChatFactory for GatedFallbackFactory {
        async fn build(
            &self,
            snapshot: SideChatParentSnapshot,
            sink: Arc<dyn ClientEventSink>,
            permission: Arc<dyn PermissionRequestSink>,
        ) -> Result<SideChatSession, String> {
            let mut session = Factory
                .build(snapshot.clone(), sink.clone(), permission)
                .await?;
            let events = vec![
                Ok(orchestrator::test_support::message_start(
                    "fallback",
                    &snapshot.config.model,
                )),
                Ok(orchestrator::test_support::content_block_start_text(0)),
                Ok(orchestrator::test_support::text_delta(0, "partial")),
                Err(llm_runtime::LlmError::Overloaded { repeated: false }),
            ];
            session.orchestrator =
                ConversationOrchestrator::into_shared(ConversationOrchestrator::new_side_chat(
                    snapshot.config.clone(),
                    Arc::new(GatedFallbackApi(self.0.clone())),
                    Arc::new(if self.1 {
                        orchestrator::test_support::MockStreamingApiClient::with_open_error(
                            llm_runtime::LlmError::ContextOverflow { token_gap: 0 },
                            vec![],
                        )
                    } else {
                        orchestrator::test_support::MockStreamingApiClient::with_fallible_turns(
                            vec![events],
                        )
                    }),
                    Arc::new(tool_api::ToolRegistry::new()),
                    noop_hook_executor(),
                    session.permission_gate.clone(),
                    Arc::new(client::adapter::AdapterOutputStream::new(sink)),
                    Arc::new(StaticMemoryProvider::empty()),
                    snapshot.cwd.clone(),
                ));
            session.orchestrator.seed_side_chat(&snapshot).await?;
            Ok(session)
        }
    }
    #[tokio::test]
    async fn stop_and_end_cancel_pending_nonstream_fallback_request() {
        for (ptl, end) in [(false, false), (false, true), (true, false), (true, true)] {
            let entered = Arc::new(Notify::new());
            let manager = SideChatManager::new(
                parent(),
                Arc::new(GatedFallbackFactory(entered.clone(), ptl)),
                Arc::new(Sink::default()),
            );
            let id = manager
                .create("fallback".into(), Some("first".into()), None, None)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(15), entered.notified())
                .await
                .unwrap();
            if end {
                tokio::time::timeout(Duration::from_secs(15), manager.end(&id))
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                tokio::time::timeout(
                    Duration::from_secs(15),
                    manager.submit(
                        &id,
                        "stop".into(),
                        ClientCommand::Cancel { turn_id: Some(1) },
                    ),
                )
                .await
                .unwrap()
                .unwrap();
            }
            manager.shutdown().await.unwrap();
        }
    }
    #[tokio::test]
    async fn initial_input_uses_native_first_turn_correlator_and_can_be_stopped() {
        let sink = Arc::new(Sink::default());
        let entered = Arc::new(Notify::new());
        let manager = SideChatManager::new(parent(), Arc::new(GatedFactory(entered.clone())), sink);
        let id = manager
            .create("create".into(), Some("first input".into()), None, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            manager
                .side(&id)
                .await
                .unwrap()
                .state
                .lock()
                .await
                .running
                .as_ref()
                .unwrap()
                .id,
            Some(1)
        );
        tokio::time::timeout(
            Duration::from_secs(15),
            manager.submit(
                &id,
                "stop".into(),
                ClientCommand::Cancel { turn_id: Some(1) },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(manager
            .side(&id)
            .await
            .unwrap()
            .state
            .lock()
            .await
            .running
            .is_none());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mismatched_turn_cancel_does_not_cancel_running_turn() {
        let entered = Arc::new(Notify::new());
        let manager = SideChatManager::new(
            parent(),
            Arc::new(GatedFactory(entered.clone())),
            Arc::new(Sink::default()),
        );
        let id = manager
            .create("create".into(), Some("first input".into()), None, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), entered.notified())
            .await
            .unwrap();

        let side = manager.side(&id).await.unwrap();
        let (running_id, running_cancel) = {
            let state = side.state.lock().await;
            let running = state.running.as_ref().unwrap();
            (running.id, running.cancel.clone())
        };
        assert_eq!(running_id, Some(1));

        manager
            .submit(
                &id,
                "stale-stop".into(),
                ClientCommand::Cancel { turn_id: Some(2) },
            )
            .await
            .unwrap();

        assert!(!running_cancel.is_cancelled());
        assert_eq!(
            side.state
                .lock()
                .await
                .running
                .as_ref()
                .and_then(|running| running.id),
            Some(1)
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slash_cancel_after_side_chat_ack_before_model_emits_correlated_terminal_result() {
        use command_api::model::{
            CommandFrontmatter, CommandSource, NativeCommandType, SlashCommand, SlashCommandKind,
        };

        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        let side = manager.side(&id).await.unwrap();
        side.session
            .dispatcher
            .registry()
            .write()
            .await
            .register_command(SlashCommand {
                name: "review".into(),
                description: "test prompt command".into(),
                source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
                native_type: Some(NativeCommandType::Prompt),
                kind: SlashCommandKind::Markdown {
                    file_path: std::path::PathBuf::from("/tmp/review.md"),
                    frontmatter: CommandFrontmatter::default(),
                    prompt_template: "Review the attached image.".into(),
                },
                ..SlashCommand::default()
            });

        manager
            .submit(
                &id,
                "slash-image".into(),
                ClientCommand::RunSlashCommand {
                    raw: "/review".into(),
                    images: vec![client::protocol::commands::ImageRefDto {
                        media_type: "image/png".into(),
                        base64: "AQID".into(),
                    }],
                    turn_id: Some(501),
                },
            )
            .await
            .unwrap();

        let cancel = side
            .state
            .lock()
            .await
            .running
            .as_ref()
            .expect("the admitted slash owns a running slot")
            .cancel
            .clone();
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if side.state.lock().await.running.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pre-model cancellation releases the Side Chat turn slot");

        let events = sink.0.lock().await.clone();
        let acknowledged = events
            .iter()
            .position(|event| {
                matches!(event,
                    ClientEvent::SideChatSubmitted { side_id, request_id }
                        if side_id == &id && request_id == "slash-image"
                )
            })
            .expect("the request is acknowledged before dispatch");
        let nested = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                ClientEvent::SideChatEvent {
                    side_id,
                    event_json,
                } if side_id == &id => serde_json::from_str::<ClientEvent>(event_json)
                    .ok()
                    .map(|inner| (index, inner)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (terminal_index, terminal) = nested
            .iter()
            .find(|(_, event)| {
                matches!(
                    event,
                    ClientEvent::SlashCommandResult {
                        turn_id: Some(501),
                        is_error: true,
                        ..
                    }
                )
            })
            .expect("the canceled pre-model slash has a correlated terminal result");
        assert!(
            acknowledged < *terminal_index,
            "the terminal result follows admission ack"
        );
        assert!(
            !nested.iter().any(|(_, event)| matches!(
                event,
                ClientEvent::TurnStarted { .. }
                    | ClientEvent::MessageComplete {
                        message: Some(_),
                        ..
                    }
            )),
            "a cancelled request must not publish a model turn or its image row"
        );
        assert!(matches!(terminal,
            ClientEvent::SlashCommandResult { display, .. }
                if display.display() == "Slash command was cancelled before entering the model."
        ));

        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slash_cancel_during_prompt_dispatch_emits_terminal_before_user_row() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        let side = manager.side(&id).await.unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(Notify::new());
        side.session
            .dispatcher
            .registry()
            .write()
            .await
            .register_builtin_handler(Arc::new(TestPromptHandler {
                entered: tokio::sync::Mutex::new(Some(entered_tx)),
                release: Some(release.clone()),
            }));

        manager
            .submit(
                &id,
                "cancel-during-dispatch".into(),
                ClientCommand::RunSlashCommand {
                    raw: "/review".into(),
                    images: Vec::new(),
                    turn_id: Some(502),
                },
            )
            .await
            .unwrap();
        entered_rx
            .await
            .expect("the command dispatcher enters the blocked Native prompt handler");
        side.state
            .lock()
            .await
            .running
            .as_ref()
            .expect("the admitted slash retains its running slot")
            .cancel
            .cancel();
        release.notify_one();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if side.state.lock().await.running.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the cancelled dispatch releases the Side Chat turn slot");

        let events = sink.0.lock().await.clone();
        let acknowledged = events
            .iter()
            .position(|event| {
                matches!(event,
                    ClientEvent::SideChatSubmitted { side_id, request_id }
                        if side_id == &id && request_id == "cancel-during-dispatch"
                )
            })
            .expect("the admitted request is acknowledged");
        let nested = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                ClientEvent::SideChatEvent {
                    side_id,
                    event_json,
                } if side_id == &id => serde_json::from_str::<ClientEvent>(event_json)
                    .ok()
                    .map(|inner| (index, inner)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            nested
                .iter()
                .filter(|(_, event)| matches!(
                    event,
                    ClientEvent::SlashCommandResult {
                        turn_id: Some(502),
                        ..
                    }
                ))
                .count(),
            1,
            "the cancelled dispatch emits exactly one correlated terminal"
        );
        let (terminal_index, terminal) = nested
            .iter()
            .find(|(_, event)| {
                matches!(
                    event,
                    ClientEvent::SlashCommandResult {
                        turn_id: Some(502),
                        is_error: true,
                        ..
                    }
                )
            })
            .expect("cancellation during dispatch has a correlated error result");
        assert!(acknowledged < *terminal_index);
        assert!(!nested.iter().any(|(_, event)| matches!(
            event,
            ClientEvent::TurnStarted { .. }
                | ClientEvent::MessageComplete {
                    message: Some(_),
                    ..
                }
        )));
        assert!(matches!(terminal,
            ClientEvent::SlashCommandResult { display, .. }
                if display.display() == "Slash command was cancelled before entering the model."
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_dedup_and_end_tombstone_never_resurrect() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("request-1".into(), None, None, None)
            .await
            .unwrap();
        assert_eq!(
            manager
                .create("request-1".into(), None, None, None)
                .await
                .unwrap(),
            id
        );
        assert_eq!(manager.list().await, vec![id.clone()]);
        manager.end(&id).await.unwrap();
        assert!(manager
            .create("request-1".into(), None, None, None)
            .await
            .is_err());
        assert!(manager.snapshot(&id).await.is_err());
        manager.shutdown().await.unwrap();
        assert!(manager
            .create("request-2".into(), None, None, None)
            .await
            .is_err());
    }
    #[test]
    fn approval_wait_is_idle_and_work_is_active() {
        let mut activity = Activity::new();
        activity.active = true;
        let future = activity.idle_since + Duration::from_secs(1801);
        assert!(!activity.expired(future, Duration::from_secs(1800)));
        activity.wait();
        let future = activity.idle_since + Duration::from_secs(1801);
        assert!(activity.expired(future, Duration::from_secs(1800)));
        activity.resume();
        assert!(!activity.expired(future, Duration::from_secs(1800)));
    }
    struct SnapshotBarrierSink {
        events: Mutex<Vec<ClientEvent>>,
        entered: Notify,
        release: Notify,
    }
    #[async_trait]
    impl ClientEventSink for SnapshotBarrierSink {
        async fn emit(&self, event: ClientEvent) {
            if matches!(event, ClientEvent::SideChatSnapshot { .. }) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.events.lock().await.push(event);
        }
    }
    #[tokio::test]
    async fn snapshot_delivery_precedes_concurrent_live_event() {
        let sink = Arc::new(SnapshotBarrierSink {
            events: Mutex::new(vec![]),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        let replay = manager.side(&id).await.unwrap().replay.clone();
        let snapshot_manager = manager.clone();
        let snapshot_id = id.clone();
        let snapshot = tokio::spawn(async move {
            snapshot_manager.emit_snapshot(&snapshot_id).await.unwrap();
        });
        sink.entered.notified().await;
        let live = tokio::spawn(async move {
            replay
                .emit(ClientEvent::SystemNotice {
                    message: "new".into(),
                    is_error: false,
                })
                .await;
        });
        tokio::task::yield_now().await;
        assert!(!sink
            .events
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, ClientEvent::SideChatEvent { .. })));
        sink.release.notify_one();
        snapshot.await.unwrap();
        live.await.unwrap();
        let events = sink.events.lock().await;
        let snapshot = events
            .iter()
            .position(|event| matches!(event, ClientEvent::SideChatSnapshot { .. }))
            .unwrap();
        let live = events
            .iter()
            .position(|event| matches!(event, ClientEvent::SideChatEvent { .. }))
            .unwrap();
        assert!(snapshot < live);
        drop(events);
        manager.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn expired_submission_is_rejected_before_timer_tick() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        manager
            .side(&id)
            .await
            .unwrap()
            .replay
            .activity
            .lock()
            .unwrap()
            .idle_since = Instant::now() - Duration::from_secs(1801);
        assert!(manager
            .submit(&id, "expired".into(), ClientCommand::ListModels)
            .await
            .is_err());
        assert!(manager.side(&id).await.is_err());
        assert!(sink
            .0
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,ClientEvent::SideChatEnded{side_id} if side_id==&id)));
        manager.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn invalid_initial_input_is_not_published_and_failure_is_deduplicated() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        assert!(manager
            .create("nested".into(), Some("/side again".into()), None, None)
            .await
            .is_err());
        assert!(manager
            .create("nested".into(), None, None, None)
            .await
            .is_err());
        assert!(manager
            .create("heavy".into(), Some("/loop 5m work".into()), None, None)
            .await
            .is_err());
        assert!(manager.list().await.is_empty());
        assert!(!sink
            .0
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, ClientEvent::SideChatCreated { .. })));
        manager.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn shutdown_closes_every_child_after_one_cleanup_failure() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink);
        let first = manager
            .create("first".into(), None, None, None)
            .await
            .unwrap();
        let second = manager
            .create("second".into(), None, None, None)
            .await
            .unwrap();
        manager
            .side(&first)
            .await
            .unwrap()
            .session
            .cleanup_failed
            .store(true, std::sync::atomic::Ordering::Release);
        let side = manager.side(&second).await.unwrap();
        let cancel = CancellationToken::new();
        let done = Arc::new(Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        side.state.lock().await.running = Some(RunningTurn {
            id: None,
            cancel: cancel.clone(),
            done: done.clone(),
            finished: finished.clone(),
        });
        tokio::spawn(async move {
            cancel.cancelled().await;
            finished.store(true, std::sync::atomic::Ordering::Release);
            done.notify_waiters();
        });
        assert!(manager.shutdown().await.is_err());
        assert!(manager.side(&second).await.is_err());
        assert_eq!(manager.info(&first).await.unwrap().status, "closing");
        assert!(manager
            .submit(&first, "late".into(), ClientCommand::ListModels)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn native_force_compact_is_admitted_and_terminal_failure_releases_its_slot() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        manager
            .submit(&id, "compact".into(), ClientCommand::ForceCompact)
            .await
            .unwrap();
        let side = manager.side(&id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if side.state.lock().await.running.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let events = side.replay.events.lock().await;
        assert!(events.iter().any(|event| matches!(event,
            ClientEvent::Error { message, .. } if message.display().starts_with("force_compact failed:")
        )));
        drop(events);
        assert!(sink.0.lock().await.iter().any(|event| matches!(event,
            ClientEvent::SideChatSubmitted { request_id, .. } if request_id == "compact"
        )));
        manager
            .submit(&id, "list".into(), ClientCommand::ListModels)
            .await
            .unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_without_running_turn_acknowledges_and_deduplicates() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        manager
            .submit(&id, "stop".into(), ClientCommand::Cancel { turn_id: None })
            .await
            .unwrap();
        manager
            .submit(&id, "stop".into(), ClientCommand::Cancel { turn_id: None })
            .await
            .unwrap();
        assert_eq!(sink.0.lock().await.iter().filter(|event|matches!(event,ClientEvent::SideChatSubmitted{request_id,..} if request_id=="stop")).count(),2);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn slash_prompt_with_images_replays_its_attachment_when_handled_successfully() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("create".into(), None, None, None)
            .await
            .unwrap();
        let side = manager.side(&id).await.unwrap();
        side.session
            .dispatcher
            .registry()
            .write()
            .await
            .register_builtin_handler(Arc::new(TestPromptHandler {
                entered: tokio::sync::Mutex::new(None),
                release: None,
            }));
        manager
            .submit(
                &id,
                "image-slash".into(),
                ClientCommand::SendPrompt {
                    text: "/review".into(),
                    prompt_mode: None,
                    images: vec![client::protocol::commands::ImageRefDto {
                        media_type: "image/png".into(),
                        base64: "AQID".into(),
                    }],
                    turn_id: Some(42),
                    visualization_context: None,
                },
            )
            .await
            .unwrap();

        let replayed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let replay = manager.snapshot(&id).await.unwrap();
                let completed = replay.iter().any(|event| {
                    matches!(
                        event,
                        ClientEvent::SlashCommandResult {
                            turn_id: Some(42),
                            is_error: false,
                            ..
                        }
                    )
                });
                let event = replay.iter().find_map(|event| match event {
                    ClientEvent::MessageComplete {
                        message: Some(message),
                        ..
                    } if message.role == "user" => Some(message.clone()),
                    _ => None,
                });
                if let Some(message) = event.filter(|_| completed) {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("image slash admission should publish its user attachment");
        assert_eq!(replayed.images.len(), 1);
        assert_eq!(replayed.images[0].media_type, "image/png");
        assert_eq!(replayed.images[0].url, "data:image/png;base64,AQID");
        assert!(sink.0.lock().await.iter().any(|event| match event {
            ClientEvent::SideChatEvent {
                side_id,
                event_json,
            } if side_id == &id => {
                serde_json::from_str::<ClientEvent>(event_json).is_ok_and(|inner| {
                    matches!(
                        inner,
                        ClientEvent::SlashCommandResult {
                            turn_id: Some(42),
                            is_error: false,
                            ..
                        }
                    )
                })
            }
            _ => false,
        }));

        manager.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn forbidden_nested_side_does_not_ack_or_mutate_admission() {
        let sink = Arc::new(Sink::default());
        let manager = SideChatManager::new(parent(), Arc::new(Factory), sink.clone());
        let id = manager
            .create("request-1".into(), None, None, None)
            .await
            .unwrap();
        assert!(manager
            .submit(
                &id,
                "submit-1".into(),
                ClientCommand::RunSlashCommand {
                    raw: "/side hi".into(),
                    images: Vec::new(),
                    turn_id: Some(1)
                }
            )
            .await
            .is_err());
        assert!(!sink
            .0
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, ClientEvent::SideChatSubmitted { .. })));
        assert!(manager
            .side(&id)
            .await
            .unwrap()
            .state
            .lock()
            .await
            .submissions
            .is_empty());
        manager.shutdown().await.unwrap();
    }
}
