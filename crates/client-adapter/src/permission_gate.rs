//! `AdapterPermissionGate` — the id-keyed, fail-closed `platform_api::PermissionGate`
//! impl (plan F1-14).
//!
//! This is the hardest mapping in the adapter: it replicates the proven
//! `TuiPermissionGate` (`tui/src/permission_bridge.rs:64-118`, COPIED not
//! imported per plan F1-10) but is **keyed by `request_id`** so a single
//! connection can multiplex concurrent worker + main permission round-trips,
//! with an explicit **fail-closed owner** so a vanished resolving task can never
//! hang the turn future.
//!
//! ## Shape (mirrors `TuiPermissionGate`, id-keyed)
//!
//! `check(name, &Value)` ([`platform_api::PermissionGate`]):
//! 1. Consult the session rules — short-circuit `Allow` if a rule matches the
//!    bare tool name (same as the TUI gate).
//! 2. Build a [`PermissionKindDto::ToolUseConfirm`] from `permission::tool_default`
//!    (the `PromptDefault` → `default_allow: bool` collapse, reusing
//!    [`crate::lowering::prompt_default_to_allow`]) and the lowered tool input
//!    JSON string ([`crate::lowering::value_to_json_string`]).
//! 3. Assign a fresh `request_id` via an [`AtomicU64`], park a
//!    `oneshot::Sender<PermissionResponse>` in the id-keyed `HashMap`, and emit a
//!    [`PermissionRequest`] DTO through the [`PermissionRequestSink`].
//! 4. `await` the oneshot, **bounded by a per-request timeout**. A dropped sender
//!    (the resolving task vanished / the map was drained) OR a timeout both
//!    resolve to [`PermissionDecision::Deny`] — fail-closed.
//!
//! `resolve(request_id, PermissionResponseDto)` is called by the transport from
//! a DIFFERENT task on an inbound `ApprovePermission`/`DenyPermission` command:
//! it looks up the id, sends on the parked oneshot, and (for `AllowAlways`)
//! appends a session [`PermissionRule`].
//!
//! ## Fail-closed owner
//!
//! The gate owns the id→sender `HashMap`. On transport teardown / app-background
//! the connection drops the gate (or calls [`AdapterPermissionGate::drain`]),
//! which drops every parked sender ⇒ every in-flight `check()` resolves `Deny`.
//! A [`Drop`] impl drains as a backstop. Combined with the per-request timeout,
//! no parked turn future can hang forever.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use client_protocol::events::ClientEvent;
use client_protocol::permission::{
    AutoModePromptDto, PermissionKindDto, PermissionOwnerDto,
    PermissionRequest as PermissionRequestDto, PermissionResolutionDto, PermissionResponseDto,
    WorkerInfoDto,
};
use permission::gate::{
    AutoModePrompt, PermissionCheckContext, PermissionDecision, PermissionGate, PermissionOutcome,
    PermissionResponse, PromptWorker,
};
use permission::{
    persist_permission_update, PermissionPaths, PermissionRule, PermissionUpdate,
    PermissionUpdateDestination,
};
use tokio::sync::{oneshot, Mutex};

use crate::lowering::{prompt_default_to_allow, value_to_json_string};
use crate::ClientEventSink;

/// Default per-request timeout — a `check()` parked longer than this resolves
/// `Deny` (fail-closed). Generous enough that a real user dialog round-trip
/// never trips it, short enough that a vanished resolving task does not hang the
/// turn future indefinitely.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// Transport-supplied destination for the outbound [`PermissionRequestDto`].
///
/// `PermissionRequest` is NOT a [`client_protocol::events::ClientEvent`] variant
/// (the frozen F1-04 contract surfaces it as its own permission DTO), so it does
/// NOT travel over the [`crate::ClientEventSink`]. The gate instead pushes each
/// request through this dedicated seam; the transport wraps it into a
/// `Frame::Event(PermissionRequest)` (bridge-server, F2-06) or hands it to the
/// `ClientEventListener` (mobile, F3). Like the event sink, it is object-safe and
/// always held behind an `Arc<dyn PermissionRequestSink>`.
#[async_trait]
pub trait PermissionRequestSink: Send + Sync {
    /// Forward one fully-lowered permission request to the underlying transport.
    /// Implementations should be cheap / non-blocking on the engine task.
    async fn emit_request(&self, request: PermissionRequestDto);
}

/// One parked permission round-trip: the oneshot the awaiting `check()` blocks
/// on, plus the call's tool input (so `resolve` can narrow an `AllowAlways`).
struct ParkedRequest {
    /// Resolves the awaiting `check()` future.
    sender: oneshot::Sender<PermissionResponse>,
    /// The tool input of the call, captured at `check()` time.
    input: serde_json::Value,
    /// Canonical tool name captured with the request; resolution never trusts
    /// a late transport-side lookup that may have timed out or been cancelled.
    tool_name: String,
    /// Immutable owner used for targeted turn cancellation.
    owner: Option<PermissionOwnerScope>,
    /// A suppressed request may be answered AllowAlways by an older or
    /// malicious client, but that response must be downgraded to AllowOnce.
    suppress_always_allow_rule: bool,
    auto_mode_prompt: Option<AutoModePrompt>,
}

#[derive(Clone)]
struct PermissionOwnerScope {
    id: u64,
    wire: PermissionOwnerDto,
}

/// Cancel-safety guard for [`AdapterPermissionGate::check_with_context_impl`]
/// (G007).
///
/// The awaiting `check()` future is not always polled to completion: a caller
/// can drop it mid-flight (a panel task hard-aborted, a subagent tool
/// dispatch cancelled, ...) while its permission ask is still parked. Nothing
/// else then removes that `pending` entry or notifies the transport — the
/// per-request timeout that would eventually resolve it lives INSIDE the very
/// future that just got dropped, so it never fires either. This guard,
/// constructed right after the entry is parked, removes it and emits a
/// `Cancelled` resolution on early drop. It is disarmed once the parked
/// oneshot resolves through the normal path (an explicit `resolve()`,
/// `cancel_owner`, `drain`, or `check_with_context_impl`'s own timeout
/// branch), all of which already own the entry's removal/notification.
struct ParkedRequestGuard {
    pending: Arc<Mutex<HashMap<u64, ParkedRequest>>>,
    event_sink: Option<Arc<dyn ClientEventSink>>,
    request_id: u64,
    armed: bool,
}

impl Drop for ParkedRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Cleanup is async (the map is a tokio Mutex and the sink emit is
        // itself async); hand it to the current runtime best-effort. If no
        // runtime is active (shutdown) there is nothing left to notify.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pending = self.pending.clone();
            let event_sink = self.event_sink.clone();
            let request_id = self.request_id;
            handle.spawn(async move {
                // Best-effort: the entry may already be gone (raced by a
                // concurrent `resolve()`/`drain()`/`cancel_owner()`), in
                // which case that caller already emitted its own resolution
                // and this is a no-op.
                let removed = pending.lock().await.remove(&request_id).is_some();
                if removed {
                    if let Some(sink) = event_sink {
                        sink.emit(ClientEvent::PermissionRequestResolved {
                            request_id,
                            resolution: PermissionResolutionDto::Cancelled,
                        })
                        .await;
                    }
                }
            });
        }
    }
}

/// The id-keyed, fail-closed permission gate the orchestrator binds as its
/// `Arc<dyn PermissionGate>` on a client connection.
///
/// Connection-scoped: one gate per transport connection. Cloneable handles share
/// the same parked-request map and session rules via `Arc`, so the transport read
/// task ([`Self::resolve`]) and the engine turn task ([`PermissionGate::check`])
/// operate on the same state.
pub struct AdapterPermissionGate {
    /// Where outbound [`PermissionRequestDto`]s go (the transport).
    sink: Arc<dyn PermissionRequestSink>,
    /// Session-scoped allow rules. Consulted before a request is emitted;
    /// appended to on `AllowAlways`. Shared with the rest of the connection.
    session_allow_rules: Arc<Mutex<Vec<PermissionRule>>>,
    /// Monotonic `request_id` source. Each `check()` reserves a fresh id so
    /// concurrent worker + main requests never collide.
    next_id: Arc<AtomicU64>,
    next_owner_id: AtomicU64,
    /// Parked requests keyed by `request_id`. The fail-closed owner: when this map
    /// is drained (or the gate dropped), every [`ParkedRequest::sender`] drops and
    /// the matching `check()` resolves `Deny`. Each entry also carries the call's
    /// tool input so [`Self::resolve`] can NARROW an `AllowAlways` to the specific
    /// command / path / domain (the input is not echoed back on the wire).
    pending: Arc<Mutex<HashMap<u64, ParkedRequest>>>,
    active_main_owner: StdMutex<Option<PermissionOwnerScope>>,
    current_session_id: StdMutex<Option<String>>,
    event_sink: Option<Arc<dyn ClientEventSink>>,
    /// Per-request timeout — a parked `check()` that is not resolved within this
    /// window resolves `Deny`.
    timeout: Duration,
    /// (3c) Filesystem roots for persisting an `AllowAlways` choice to
    /// `settings.local.json`. `None` → session-only (the legacy behavior); when
    /// set, an `AllowAlways` additionally writes a durable allow rule.
    persist_paths: Option<PermissionPaths>,
    persistence_enabled: AtomicBool,
}

impl AdapterPermissionGate {
    /// Construct a gate with the [`DEFAULT_PERMISSION_TIMEOUT`] and a fresh empty
    /// session-rule list.
    #[must_use]
    pub fn new(sink: Arc<dyn PermissionRequestSink>) -> Self {
        Self::with_rules(sink, Arc::new(Mutex::new(Vec::new())))
    }

    /// Construct a gate sharing an existing session-rule list (the connection's
    /// canonical list, also surfaced to `/permissions`).
    #[must_use]
    pub fn with_rules(
        sink: Arc<dyn PermissionRequestSink>,
        session_allow_rules: Arc<Mutex<Vec<PermissionRule>>>,
    ) -> Self {
        Self {
            sink,
            session_allow_rules,
            next_id: Arc::new(AtomicU64::new(1)),
            next_owner_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            active_main_owner: StdMutex::new(None),
            current_session_id: StdMutex::new(None),
            event_sink: None,
            timeout: DEFAULT_PERMISSION_TIMEOUT,
            persist_paths: None,
            persistence_enabled: AtomicBool::new(true),
        }
    }

    /// Override the per-request timeout (tests use a short window for
    /// `timeout_resolves_deny`).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Emit authoritative permission terminal events through the client stream.
    #[must_use]
    pub fn with_event_sink(mut self, sink: Arc<dyn ClientEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Update the mounted session used for newly-created worker owner records.
    pub fn set_session_id(&self, session_id: Option<String>) {
        *self
            .current_session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = session_id;
    }

    /// Begin one main-agent turn and return its private cancellation owner id.
    pub fn begin_main_turn(&self, session_id: Option<String>, turn_id: Option<u64>) -> u64 {
        let id = self.next_owner_id.fetch_add(1, Ordering::Relaxed);
        let owner = PermissionOwnerScope {
            id,
            wire: PermissionOwnerDto {
                session_id,
                turn_id,
                worker_name: None,
            },
        };
        *self
            .active_main_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(owner);
        id
    }

    /// Clear the main owner iff it still names the completed turn.
    pub fn end_main_turn(&self, owner_id: u64) {
        let mut active = self
            .active_main_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.as_ref().is_some_and(|owner| owner.id == owner_id) {
            *active = None;
        }
    }

    async fn emit_resolution(&self, request_id: u64, resolution: PermissionResolutionDto) {
        if let Some(sink) = &self.event_sink {
            sink.emit(ClientEvent::PermissionRequestResolved {
                request_id,
                resolution,
            })
            .await;
        }
    }

    /// (3c) Enable persisting an `AllowAlways` choice to `settings.local.json`
    /// (under `paths.cwd/.lingxi/`). Without this, `AllowAlways` is session-only.
    #[must_use]
    pub fn with_persist(mut self, paths: PermissionPaths) -> Self {
        self.persist_paths = Some(paths);
        self
    }

    /// Shared handle to the session-allow-rule list (so the connection can also
    /// read/surface it).
    #[must_use]
    pub fn session_allow_rules(&self) -> Arc<Mutex<Vec<PermissionRule>>> {
        self.session_allow_rules.clone()
    }

    /// Resolve a parked request by `request_id` from a DIFFERENT task (the
    /// transport read loop on an inbound `ApprovePermission`/`DenyPermission`).
    ///
    /// Looks up and removes the parked sender, then sends the mapped
    /// [`PermissionResponse`]. `AllowAlways` additionally appends a session
    /// [`PermissionRule`] for `tool_name` so subsequent same-tool calls skip the
    /// dialog. Returns `true` if a matching request was found and resolved,
    /// `false` if the id was unknown / already resolved.
    pub async fn resolve(
        &self,
        request_id: u64,
        response: PermissionResponseDto,
        _tool_name_hint: &str,
    ) -> bool {
        let parked = { self.pending.lock().await.remove(&request_id) };
        let Some(ParkedRequest {
            sender,
            input,
            tool_name,
            suppress_always_allow_rule,
            auto_mode_prompt,
            ..
        }) = parked
        else {
            return false;
        };

        let allow_always =
            matches!(response, PermissionResponseDto::AllowAlways) && !suppress_always_allow_rule;
        let mapped = match response {
            PermissionResponseDto::AllowOnce => PermissionResponse::AllowOnce,
            PermissionResponseDto::AllowAlways if suppress_always_allow_rule => {
                // A stale client may still submit the removed AllowAlways
                // action. Preserve this explicit approval for the current call,
                // but never let it create a session or durable rule.
                PermissionResponse::AllowOnce
            }
            PermissionResponseDto::AllowAlways => PermissionResponse::AllowAlways,
            PermissionResponseDto::AllowAuto
                if auto_mode_prompt.is_some() && !suppress_always_allow_rule =>
            {
                PermissionResponse::AllowAuto
            }
            // A stale or malicious client cannot switch mode unless the engine
            // marked this request eligible. Keep its explicit approval as a
            // one-shot grant, with no persistence or mode transition.
            PermissionResponseDto::AllowAuto => PermissionResponse::AllowOnce,
            // `#[non_exhaustive]` — any future/`Deny` response fails closed.
            _ => PermissionResponse::Deny,
        };
        // The parked receiver is the turn-ownership proof. A cancellation drain
        // or timeout drops it; in that case a late AllowAlways must not create a
        // session or durable rule for a tool call that no longer exists.
        //
        // [round-2 review, finding 8] This branch already removed the
        // `pending` entry (the `remove` above) before discovering the
        // receiver is dead, so `ParkedRequestGuard`'s backstop (armed only
        // while the entry is still IN the map) finds nothing to clean up and
        // emits nothing either. Without an explicit emit here, a resolve()
        // that races a dropped asker produces NO
        // `PermissionRequestResolved` event at all — on a client that does
        // not optimistically dequeue (Android), the permission card is then
        // stuck on screen until connection-teardown `drain()`. Emit the same
        // terminal event the guard would have.
        if sender.send(mapped).is_err() {
            self.emit_resolution(request_id, PermissionResolutionDto::Cancelled)
                .await;
            return false;
        }

        if allow_always {
            // NARROW the persisted grant to the specific command / path / domain
            // the call used (claude-code `ruleSuggestions`), not a tool-wide allow.
            let rule = permission::allow_suggestion(&tool_name, &input);
            self.session_allow_rules.lock().await.push(rule.clone());
            // (3c) Durably record the choice to settings.local.json when a
            // persist target is wired. Best-effort: a write failure must not
            // fail the resolve (the session rule above still skips re-prompts).
            // Skip a degenerate empty tool name (a missing request id resolves
            // `tool_name = ""`) so we never persist `allow: [""]`.
            if let Some(paths) = self.persist_paths.as_ref().filter(|_| {
                !tool_name.is_empty() && self.persistence_enabled.load(Ordering::Acquire)
            }) {
                let update = PermissionUpdate {
                    rule,
                    destination: PermissionUpdateDestination::LocalSettings,
                };
                if let Err(e) = persist_permission_update(&update, paths).await {
                    tracing::warn!(
                        tool = %tool_name,
                        error_kind = std::any::type_name_of_val(&e),
                        "failed to persist AllowAlways permission rule"
                    );
                }
            }
        }

        let resolution = if matches!(response, PermissionResponseDto::Deny) {
            PermissionResolutionDto::Denied
        } else {
            PermissionResolutionDto::Approved
        };
        self.emit_resolution(request_id, resolution).await;

        true
    }

    /// Cancel only requests owned by one main turn, preserving child agents.
    pub async fn cancel_owner(&self, owner_id: u64) -> Vec<u64> {
        let removed = {
            let mut pending = self.pending.lock().await;
            let ids = pending
                .iter()
                .filter_map(|(request_id, parked)| {
                    parked
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.id == owner_id)
                        .then_some(*request_id)
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|request_id| {
                    pending
                        .remove(&request_id)
                        .map(|parked| (request_id, parked))
                })
                .collect::<Vec<_>>()
        };
        let request_ids = removed
            .iter()
            .map(|(request_id, _)| *request_id)
            .collect::<Vec<_>>();
        drop(removed);
        for request_id in &request_ids {
            self.emit_resolution(*request_id, PermissionResolutionDto::Cancelled)
                .await;
        }
        request_ids
    }

    /// Fail-closed drain: drop every parked sender so all in-flight `check()`
    /// calls resolve `Deny`. Called on transport teardown / app-background.
    /// Returns the number of requests that were drained.
    pub async fn drain(&self) -> usize {
        let mut pending = self.pending.lock().await;
        let request_ids = pending.keys().copied().collect::<Vec<_>>();
        pending.clear();
        drop(pending);
        for request_id in &request_ids {
            self.emit_resolution(*request_id, PermissionResolutionDto::Cancelled)
                .await;
        }
        request_ids.len()
    }

    /// Number of requests currently parked (test/inspection helper).
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

impl Drop for AdapterPermissionGate {
    fn drop(&mut self) {
        // Backstop drain: drop every parked sender so any still-awaiting
        // `check()` future resolves `Deny`. `get_mut` avoids needing an async
        // context in `drop` — we hold the only `&mut self` here.
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.clear();
        }
        // If the lock is contended (another clone is mid-`resolve`/`check`), the
        // remaining `Arc`s keep the map alive; the LAST holder's drop (or an
        // explicit `drain`) clears it. The per-request timeout is the final
        // backstop regardless.
    }
}

impl AdapterPermissionGate {
    async fn check_with_context_impl(
        &self,
        name: &str,
        input: &serde_json::Value,
        worker: Option<PromptWorker>,
        suppress_always_allow_rule: bool,
        auto_mode_prompt: Option<AutoModePrompt>,
    ) -> PermissionOutcome {
        // Step 1: consult session rules (identical to the TUI gate; content-aware
        // so a narrowed AllowAlways rule only short-circuits a matching call).
        // A requiresUserInteraction tool must not be bypassed by an older rule.
        {
            let rules = self.session_allow_rules.lock().await;
            if !suppress_always_allow_rule
                && rules
                    .iter()
                    .any(|r| permission::call_matches_rule(r, name, input))
            {
                return PermissionOutcome::Allow {
                    updated_input: None,
                    permission_updates: Vec::new(),
                    decision_classification: None,
                };
            }
        }

        let owner = if let Some(worker) = worker.as_ref() {
            Some(PermissionOwnerScope {
                id: self.next_owner_id.fetch_add(1, Ordering::Relaxed),
                wire: PermissionOwnerDto {
                    session_id: self
                        .current_session_id
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone(),
                    turn_id: None,
                    worker_name: Some(worker.name.clone()),
                },
            })
        } else {
            self.active_main_owner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        };

        // Step 2: build the request DTO. ExitPlanMode carries a real plan body
        // and must use its dedicated wire kind; every other tool keeps the
        // generic ToolUseConfirm shape.  Sanitize the optional Auto token at
        // this transport boundary as a second line of defense against a stale
        // caller pairing the wrong action with the request kind.
        let (kind, auto_mode_prompt) = if name == "ExitPlanMode" {
            let auto_mode_prompt = matches!(auto_mode_prompt, Some(AutoModePrompt::ExitPlanMode))
                .then_some(AutoModePrompt::ExitPlanMode);
            (
                PermissionKindDto::ExitPlanMode {
                    plan: input
                        .get("plan")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
                auto_mode_prompt,
            )
        } else {
            let auto_mode_prompt = matches!(auto_mode_prompt, Some(AutoModePrompt::WorkflowBash))
                .then_some(AutoModePrompt::WorkflowBash);
            let default_allow = prompt_default_to_allow(permission::tool_default(name));
            (
                PermissionKindDto::ToolUseConfirm {
                    tool_name: name.to_string(),
                    tool_input_json: value_to_json_string(input),
                    default_allow,
                },
                auto_mode_prompt,
            )
        };

        // Step 3: reserve a fresh id, park the oneshot, emit the request.
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().await.insert(
                request_id,
                ParkedRequest {
                    sender: tx,
                    input: input.clone(),
                    tool_name: name.to_string(),
                    owner: owner.clone(),
                    suppress_always_allow_rule,
                    auto_mode_prompt,
                },
            );
        }
        // G007: from here on the entry is parked in `self.pending`. If this
        // whole `check_with_context_impl` future gets dropped before reaching
        // the normal resolution below, nothing else ever cleans it up —
        // this guard's `Drop` is the backstop.
        let mut parked_guard = ParkedRequestGuard {
            pending: self.pending.clone(),
            event_sink: self.event_sink.clone(),
            request_id,
            armed: true,
        };
        let request = PermissionRequestDto {
            request_id,
            kind,
            // Worker attribution (claude-code 2.1.186): a subagent/teammate's
            // permission prompt carries the asking worker so the remote client
            // renders the `● @name` badge. `color` seeds the multiagent color
            // (the worker's display name); `None` for a main-thread call.
            worker: worker.map(|w| WorkerInfoDto {
                color: w.name.clone(),
                name: w.name,
                team: w.team,
            }),
            owner: owner.map(|owner| owner.wire),
            suppress_always_allow_rule,
            auto_mode_prompt: auto_mode_prompt.map(|prompt| match prompt {
                AutoModePrompt::WorkflowBash => AutoModePromptDto::WorkflowBash,
                AutoModePrompt::ExitPlanMode => AutoModePromptDto::ExitPlanMode,
            }),
        };
        self.sink.emit_request(request).await;

        // Step 4: await the resolution, bounded by the per-request timeout.
        // A dropped sender (drain / vanished resolver) OR a timeout both fail
        // closed to `Deny`. On timeout we also evict the now-stale parked entry
        // so a late `resolve()` is a no-op (and the map does not leak).
        let timeout_result = tokio::time::timeout(self.timeout, rx).await;
        // The oneshot resolved (or the deadline elapsed). Disarm the guard
        // separately in each arm below, no earlier than the point where the
        // entry is actually gone from `self.pending` — NOT unconditionally
        // here, before the `Err(_elapsed)` arm's own eviction has run. This
        // future can still be dropped between here and that arm's
        // `self.pending.lock().await` (a genuine suspension point when
        // another parked/resolving request contends the same map); an early
        // disarm would leave that window uncovered by both the timeout
        // eviction (never reached) and the guard (already disarmed) alike.
        let response = match timeout_result {
            Ok(Ok(response)) => {
                // A normal `resolve()` already removed the entry before
                // sending on `tx`.
                parked_guard.armed = false;
                response
            }
            Ok(Err(_dropped)) => {
                // The sender was dropped by `drain()`/`cancel_owner()`,
                // which also already removed the entry.
                parked_guard.armed = false;
                return PermissionOutcome::Deny {
                    reason: "permission request dropped (connection closed)".to_string(),
                };
            }
            Err(_elapsed) => {
                let expired = self.pending.lock().await.remove(&request_id).is_some();
                // Only now is the entry provably gone — disarm after the
                // removal, not before it.
                parked_guard.armed = false;
                if expired {
                    self.emit_resolution(request_id, PermissionResolutionDto::Expired)
                        .await;
                }
                return PermissionOutcome::Deny {
                    reason: "permission request timed out".to_string(),
                };
            }
        };

        // Step 5: map the resolved response → decision (the AllowAlways rule was
        // already persisted in `resolve`, unless this request suppressed it).
        match response {
            PermissionResponse::AllowOnce | PermissionResponse::AllowAlways => {
                PermissionOutcome::Allow {
                    updated_input: None,
                    permission_updates: Vec::new(),
                    decision_classification: None,
                }
            }
            PermissionResponse::AllowAuto
                if auto_mode_prompt.is_some() && !suppress_always_allow_rule =>
            {
                PermissionOutcome::AllowAuto {
                    updated_input: None,
                }
            }
            PermissionResponse::AllowAuto => PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
            PermissionResponse::Deny => PermissionOutcome::Deny {
                reason: "user denied via permission dialog".to_string(),
            },
        }
    }
}

#[async_trait]
impl PermissionGate for AdapterPermissionGate {
    fn set_permission_persistence_enabled(&self, enabled: bool) {
        self.persistence_enabled.store(enabled, Ordering::Release);
    }

    async fn check(&self, name: &str, input: &serde_json::Value) -> PermissionDecision {
        // Main-thread call — no worker attribution on the wire.
        self.check_with_worker(name, input, None).await
    }

    async fn check_with_worker(
        &self,
        name: &str,
        input: &serde_json::Value,
        worker: Option<PromptWorker>,
    ) -> PermissionDecision {
        match self
            .check_with_context_impl(name, input, worker, false, None)
            .await
        {
            PermissionOutcome::Allow { .. } | PermissionOutcome::AllowAuto { .. } => {
                PermissionDecision::Allow
            }
            PermissionOutcome::Deny { reason } => PermissionDecision::Deny { reason },
        }
    }

    async fn check_with_context(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        match self
            .check_with_context_impl(
                name,
                input,
                ctx.worker.clone(),
                ctx.suppress_always_allow_rule,
                ctx.auto_mode_prompt,
            )
            .await
        {
            outcome => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockSink;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    /// A [`PermissionRequestSink`] that captures every emitted request so a test
    /// can read back the assigned `request_id` and assert the lowered fields.
    #[derive(Default)]
    struct MockRequestSink {
        requests: TokioMutex<Vec<PermissionRequestDto>>,
    }

    impl MockRequestSink {
        fn arc() -> Arc<Self> {
            Arc::new(Self::default())
        }
        async fn requests(&self) -> Vec<PermissionRequestDto> {
            self.requests.lock().await.clone()
        }
        async fn last(&self) -> PermissionRequestDto {
            self.requests
                .lock()
                .await
                .last()
                .cloned()
                .expect("a request was emitted")
        }
    }

    #[async_trait]
    impl PermissionRequestSink for MockRequestSink {
        async fn emit_request(&self, request: PermissionRequestDto) {
            self.requests.lock().await.push(request);
        }
    }

    /// Spin until the gate has parked exactly `n` requests (the emit + park
    /// happens on the spawned `check()` task). Bounded so a bug can't hang.
    async fn wait_for_pending(gate: &AdapterPermissionGate, n: usize) {
        for _ in 0..2000 {
            if gate.pending_count().await == n {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("gate never reached {n} pending requests");
    }

    // ── Mirror of the 4 `permission_bridge.rs` gate tests ──────────────────

    /// `gate_emits_request_and_resolves_allow_once` — emit id N, `resolve(N,
    /// AllowOnce)`, assert `check()` returns `PermissionDecision::Allow`.
    #[tokio::test]
    async fn gate_emits_request_and_resolves_allow_once() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({"command": "ls"})).await });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;
        match &req.kind {
            PermissionKindDto::ToolUseConfirm { tool_name, .. } => assert_eq!(tool_name, "Bash"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert!(
            gate.resolve(req.request_id, PermissionResponseDto::AllowOnce, "Bash")
                .await
        );

        let decision = task.await.unwrap();
        assert_eq!(decision, PermissionDecision::Allow);
        // No rule persisted on AllowOnce.
        assert!(gate.session_allow_rules().lock().await.is_empty());
    }

    #[tokio::test]
    async fn eligible_auto_response_is_rich_outcome_without_session_rule() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));
        let ctx = PermissionCheckContext {
            auto_mode_prompt: Some(AutoModePrompt::WorkflowBash),
            ..PermissionCheckContext::default()
        };
        let g = gate.clone();
        let task = tokio::spawn(async move {
            g.check_with_context("Bash", &json!({"command": "echo hi"}), &ctx)
                .await
        });

        wait_for_pending(&gate, 1).await;
        let request = sink.last().await;
        assert_eq!(
            request.auto_mode_prompt,
            Some(AutoModePromptDto::WorkflowBash)
        );
        assert!(
            gate.resolve(request.request_id, PermissionResponseDto::AllowAuto, "Bash")
                .await
        );
        assert!(matches!(
            task.await.unwrap(),
            PermissionOutcome::AllowAuto {
                updated_input: None
            }
        ));
        assert!(gate.session_allow_rules().lock().await.is_empty());
    }

    #[tokio::test]
    async fn exit_plan_approval_uses_plan_kind_and_preserves_auto_action() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));
        let ctx = PermissionCheckContext {
            auto_mode_prompt: Some(AutoModePrompt::ExitPlanMode),
            ..PermissionCheckContext::default()
        };
        let g = gate.clone();
        let task = tokio::spawn(async move { g.check_exit_plan_mode("1. Ship it", &ctx).await });

        wait_for_pending(&gate, 1).await;
        let request = sink.last().await;
        match &request.kind {
            PermissionKindDto::ExitPlanMode { plan } => assert_eq!(plan, "1. Ship it"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(
            request.auto_mode_prompt,
            Some(AutoModePromptDto::ExitPlanMode)
        );
        assert!(
            gate.resolve(
                request.request_id,
                PermissionResponseDto::AllowOnce,
                "ExitPlanMode"
            )
            .await
        );
        assert!(matches!(
            task.await.unwrap(),
            PermissionOutcome::Allow { .. }
        ));
    }

    /// `gate_skips_dialog_when_session_rule_matches` — a pre-seeded rule
    /// short-circuits `Allow` with NO request emitted.
    #[tokio::test]
    async fn gate_skips_dialog_when_session_rule_matches() {
        let sink = MockRequestSink::arc();
        let rules = Arc::new(Mutex::new(vec![PermissionRule::allow_tool_session("Bash")]));
        let gate = AdapterPermissionGate::with_rules(sink.clone(), rules);

        let decision = gate.check("Bash", &json!({"command": "ls"})).await;
        assert_eq!(decision, PermissionDecision::Allow);
        // No request was emitted to the transport.
        assert!(sink.requests().await.is_empty());
        assert_eq!(gate.pending_count().await, 0);
    }

    /// `gate_persists_allow_always` — `AllowAlways` appends a session rule.
    #[tokio::test]
    async fn gate_persists_allow_always() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({})).await });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;
        assert!(
            gate.resolve(req.request_id, PermissionResponseDto::AllowAlways, "Bash")
                .await
        );

        let decision = task.await.unwrap();
        assert_eq!(decision, PermissionDecision::Allow);

        let stored = gate.session_allow_rules();
        let stored = stored.lock().await;
        assert_eq!(stored.len(), 1);
        assert!(stored[0].matches_tool("Bash"));
    }

    #[tokio::test]
    async fn suppressed_request_downgrades_allow_always_to_once() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));
        let ctx = PermissionCheckContext {
            suppress_always_allow_rule: true,
            ..PermissionCheckContext::default()
        };

        let g = gate.clone();
        let task = tokio::spawn(async move {
            g.check_with_context("McpTool", &json!({"value": 1}), &ctx)
                .await
        });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;
        assert!(req.suppress_always_allow_rule);
        assert!(
            gate.resolve(
                req.request_id,
                PermissionResponseDto::AllowAlways,
                "McpTool"
            )
            .await
        );
        assert!(matches!(
            task.await.unwrap(),
            PermissionOutcome::Allow { .. }
        ));
        assert!(
            gate.session_allow_rules().lock().await.is_empty(),
            "a stale AllowAlways response must not create a session rule"
        );
    }

    /// `AllowAlways` NARROWS the persisted rule to the call's command (not a bare
    /// tool-wide allow), and the narrowed rule still short-circuits a matching
    /// later call within the session.
    #[tokio::test]
    async fn gate_persists_allow_always_narrows_to_command() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        let g = gate.clone();
        let input = json!({ "command": "git commit -m \"x\"" });
        let task = tokio::spawn(async move { g.check("Bash", &input).await });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;
        assert!(
            gate.resolve(req.request_id, PermissionResponseDto::AllowAlways, "Bash")
                .await
        );
        assert_eq!(task.await.unwrap(), PermissionDecision::Allow);

        // The stored rule is narrowed to the command prefix, NOT tool-wide.
        {
            let stored = gate.session_allow_rules();
            let stored = stored.lock().await;
            assert_eq!(stored.len(), 1);
            assert_eq!(
                stored[0].value.rule_content.as_deref(),
                // `PermissionRule` keeps the shell matcher in its canonical
                // internal space-star form. The settings serializer is the
                // boundary that renders this as `Bash(git commit:*)`.
                Some("git commit *")
            );
            assert!(
                !stored[0].matches_tool("Bash"),
                "narrowed rule is not tool-wide"
            );
        }

        // A matching later command short-circuits (no new request emitted)...
        assert_eq!(
            gate.check("Bash", &json!({ "command": "git commit -m \"y\"" }))
                .await,
            PermissionDecision::Allow
        );
        // ...but a DIFFERENT command still prompts (would park a new request).
        let g2 = gate.clone();
        let other =
            tokio::spawn(async move { g2.check("Bash", &json!({ "command": "rm -rf /" })).await });
        wait_for_pending(&gate, 1).await;
        let req2 = sink.last().await;
        assert!(
            gate.resolve(req2.request_id, PermissionResponseDto::Deny, "Bash")
                .await
        );
        assert!(matches!(
            other.await.unwrap(),
            PermissionDecision::Deny { .. }
        ));
    }

    /// (3c) `gate_persists_allow_always_writes_local_settings` — with a persist
    /// target wired, `AllowAlways` ALSO writes a durable rule to
    /// `<cwd>/.lingxi/settings.local.json` (in addition to the session rule).
    #[tokio::test]
    async fn gate_persists_allow_always_writes_local_settings() {
        let tmp = std::env::temp_dir().join(format!("lx-3c-adapter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()).with_persist(
            permission::PermissionPaths {
                lingxi_home: tmp.join("home/.lingxi"),
                cwd: tmp.join("proj"),
            },
        ));

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({})).await });
        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;
        assert!(
            gate.resolve(req.request_id, PermissionResponseDto::AllowAlways, "Bash")
                .await
        );
        assert_eq!(task.await.unwrap(), PermissionDecision::Allow);

        // The choice was persisted to settings.local.json.
        let path = tmp.join("proj/.lingxi/settings.local.json");
        let written = std::fs::read_to_string(&path).expect("settings.local.json written");
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash"]));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `gate_denies_on_user_deny` — `Deny` resolves to `PermissionDecision::Deny`.
    #[tokio::test]
    async fn gate_denies_on_user_deny() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({})).await });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;
        assert!(
            gate.resolve(req.request_id, PermissionResponseDto::Deny, "Bash")
                .await
        );

        match task.await.unwrap() {
            PermissionDecision::Deny { reason } => {
                assert!(reason.contains("user denied"), "got: {reason}");
            }
            PermissionDecision::Allow => panic!("expected Deny"),
        }
        // Resolved request is no longer parked.
        assert_eq!(gate.pending_count().await, 0);
    }

    // ── NEW (plan F1-14): id-keyed concurrency + fail-closed paths ──────────

    /// `concurrent_worker_and_main_ids_resolve_independently` — two parked
    /// requests get distinct ids and resolve to independent decisions.
    #[tokio::test]
    async fn concurrent_worker_and_main_ids_resolve_independently() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        let g1 = gate.clone();
        let main = tokio::spawn(async move { g1.check("Bash", &json!({"who": "main"})).await });
        let g2 = gate.clone();
        let worker =
            tokio::spawn(async move { g2.check("Write", &json!({"who": "worker"})).await });

        wait_for_pending(&gate, 2).await;
        let reqs = sink.requests().await;
        assert_eq!(reqs.len(), 2);
        // Distinct ids.
        assert_ne!(reqs[0].request_id, reqs[1].request_id);

        // Find each by tool name, resolve them to OPPOSITE decisions.
        let bash = reqs.iter().find(|r| {
            matches!(&r.kind, PermissionKindDto::ToolUseConfirm { tool_name, .. } if tool_name == "Bash")
        }).unwrap();
        let write = reqs.iter().find(|r| {
            matches!(&r.kind, PermissionKindDto::ToolUseConfirm { tool_name, .. } if tool_name == "Write")
        }).unwrap();

        assert!(
            gate.resolve(bash.request_id, PermissionResponseDto::AllowOnce, "Bash")
                .await
        );
        assert!(
            gate.resolve(write.request_id, PermissionResponseDto::Deny, "Write")
                .await
        );

        assert_eq!(main.await.unwrap(), PermissionDecision::Allow);
        match worker.await.unwrap() {
            PermissionDecision::Deny { .. } => {}
            PermissionDecision::Allow => panic!("worker expected Deny"),
        }
    }

    #[tokio::test]
    async fn cancelling_main_owner_preserves_worker_request_and_emits_terminal_events() {
        let requests = MockRequestSink::arc();
        let events = MockSink::arc();
        let gate =
            Arc::new(AdapterPermissionGate::new(requests.clone()).with_event_sink(events.clone()));
        gate.set_session_id(Some("session-a".to_string()));
        let owner_id = gate.begin_main_turn(Some("session-a".to_string()), Some(7));

        let main_gate = gate.clone();
        let main =
            tokio::spawn(async move { main_gate.check("Bash", &json!({"command": "main"})).await });
        let worker_gate = gate.clone();
        let worker = tokio::spawn(async move {
            worker_gate
                .check_with_worker(
                    "Write",
                    &json!({"file_path": "/tmp/x"}),
                    Some(PromptWorker {
                        name: "design".to_string(),
                        team: None,
                        is_async: true,
                    }),
                )
                .await
        });

        wait_for_pending(&gate, 2).await;
        let emitted = requests.requests().await;
        let main_request = emitted
            .iter()
            .find(|request| {
                matches!(
                    &request.kind,
                    PermissionKindDto::ToolUseConfirm { tool_name, .. } if tool_name == "Bash"
                )
            })
            .expect("main request");
        let worker_request = emitted
            .iter()
            .find(|request| {
                matches!(
                    &request.kind,
                    PermissionKindDto::ToolUseConfirm { tool_name, .. } if tool_name == "Write"
                )
            })
            .expect("worker request");
        assert_eq!(
            main_request.owner.as_ref().and_then(|owner| owner.turn_id),
            Some(7)
        );
        assert_eq!(
            worker_request
                .owner
                .as_ref()
                .and_then(|owner| owner.worker_name.as_deref()),
            Some("design")
        );

        let cancelled = gate.cancel_owner(owner_id).await;
        assert_eq!(cancelled, vec![main_request.request_id]);
        assert_eq!(gate.pending_count().await, 1);
        assert!(matches!(
            main.await.unwrap(),
            PermissionDecision::Deny { .. }
        ));

        assert!(
            gate.resolve(
                worker_request.request_id,
                PermissionResponseDto::Deny,
                "Write"
            )
            .await
        );
        assert!(matches!(
            worker.await.unwrap(),
            PermissionDecision::Deny { .. }
        ));
        let terminal = events.events().await;
        assert!(terminal.iter().any(|event| matches!(
            event,
            ClientEvent::PermissionRequestResolved { request_id, resolution: PermissionResolutionDto::Cancelled }
                if *request_id == main_request.request_id
        )));
        assert!(terminal.iter().any(|event| matches!(
            event,
            ClientEvent::PermissionRequestResolved { request_id, resolution: PermissionResolutionDto::Denied }
                if *request_id == worker_request.request_id
        )));
    }

    /// `drop_resolves_deny` — draining the parked map drops the sender ⇒ the
    /// in-flight `check()` resolves `Deny` (fail-closed on transport teardown).
    #[tokio::test]
    async fn drop_resolves_deny() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({})).await });

        wait_for_pending(&gate, 1).await;
        let drained = gate.drain().await;
        assert_eq!(drained, 1);

        match task.await.unwrap() {
            PermissionDecision::Deny { reason } => {
                assert!(reason.contains("dropped"), "got: {reason}");
            }
            PermissionDecision::Allow => panic!("expected Deny after drain"),
        }
    }

    /// G007: dropping the AWAITER's own future — not the gate — while its
    /// permission ask is still parked (a panel task hard-aborted, a subagent
    /// tool dispatch cancelled, ...) must not leak the entry forever. Nothing
    /// else would ever remove it or notify the transport: the per-request
    /// timeout that would eventually resolve it lives INSIDE the very future
    /// that was just dropped, so it never fires either.
    #[tokio::test]
    async fn dropped_asker_evicts_parked_request_and_emits_cancelled() {
        let sink = MockRequestSink::arc();
        let events = MockSink::arc();
        let gate =
            Arc::new(AdapterPermissionGate::new(sink.clone()).with_event_sink(events.clone()));

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({"command": "ls"})).await });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;

        // The asker vanishes mid-flight instead of ever resolving normally —
        // e.g. a Fusion panel task hard-aborted while a Bash ask was parked.
        task.abort();
        let _ = task.await;

        for _ in 0..2000 {
            if gate.pending_count().await == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            gate.pending_count().await,
            0,
            "an aborted asker must not leak its parked permission request forever"
        );

        for _ in 0..2000 {
            if !events.events().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let seen = events.events().await;
        assert!(
            matches!(
                seen.as_slice(),
                [ClientEvent::PermissionRequestResolved {
                    request_id,
                    resolution: PermissionResolutionDto::Cancelled,
                }] if *request_id == req.request_id
            ),
            "expected exactly one Cancelled resolution for request {}; got: {seen:?}",
            req.request_id
        );
    }

    /// A response whose parked receiver has already disappeared must not
    /// convert a stale AllowAlways tap into a session or durable grant.
    #[tokio::test]
    async fn late_allow_always_does_not_write_rule() {
        let sink = MockRequestSink::arc();
        let gate = AdapterPermissionGate::new(sink);
        let (sender, receiver) = oneshot::channel();
        drop(receiver);
        gate.pending.lock().await.insert(
            41,
            ParkedRequest {
                sender,
                input: json!({"command": "dangerous"}),
                tool_name: "Bash".to_string(),
                owner: None,
                suppress_always_allow_rule: false,
                auto_mode_prompt: None,
            },
        );

        assert!(
            !gate
                .resolve(41, PermissionResponseDto::AllowAlways, "Bash")
                .await
        );
        assert!(gate.session_allow_rules.lock().await.is_empty());
    }

    /// [round-2 review, finding 8] `resolve()` already removes the `pending`
    /// entry (proven by `late_allow_always_does_not_write_rule` above)
    /// before discovering the receiver is dead. Before the fix it then
    /// returned `false` with NO terminal event at all — the entry was gone,
    /// so `ParkedRequestGuard`'s backstop (armed only while the entry is
    /// still in the map) finds nothing to clean up either, and the id's
    /// `PermissionRequestResolved` is lost forever. On a client that does
    /// not optimistically dequeue (Android), that stranded the permission
    /// card on screen until connection teardown. `resolve()` must still emit
    /// exactly one terminal event on this branch.
    #[tokio::test]
    async fn resolve_on_dead_receiver_still_emits_one_resolution_event() {
        let sink = MockRequestSink::arc();
        let events = MockSink::arc();
        let gate = AdapterPermissionGate::new(sink).with_event_sink(events.clone());
        let (sender, receiver) = oneshot::channel();
        drop(receiver);
        gate.pending.lock().await.insert(
            77,
            ParkedRequest {
                sender,
                input: json!({"command": "dangerous"}),
                tool_name: "Bash".to_string(),
                owner: None,
                suppress_always_allow_rule: false,
                auto_mode_prompt: None,
            },
        );

        let resolved = gate
            .resolve(77, PermissionResponseDto::AllowOnce, "Bash")
            .await;
        assert!(
            !resolved,
            "no live asker remains, so resolve() must still report false"
        );

        let seen = events.events().await;
        assert!(
            matches!(
                seen.as_slice(),
                [ClientEvent::PermissionRequestResolved {
                    request_id: 77,
                    resolution: PermissionResolutionDto::Cancelled,
                }]
            ),
            "resolve() racing a dropped asker must still emit exactly one terminal event; got: {seen:?}"
        );
    }

    /// `timeout_resolves_deny` — a `check()` parked past the per-request timeout
    /// resolves `Deny` even if nobody ever resolves it.
    #[tokio::test]
    async fn timeout_resolves_deny() {
        let sink = MockRequestSink::arc();
        let events = MockSink::arc();
        let gate = AdapterPermissionGate::new(sink.clone())
            .with_event_sink(events.clone())
            .with_timeout(Duration::from_millis(20));

        let decision = gate.check("Bash", &json!({})).await;
        match decision {
            PermissionDecision::Deny { reason } => {
                assert!(reason.contains("timed out"), "got: {reason}");
            }
            PermissionDecision::Allow => panic!("expected Deny on timeout"),
        }
        // The stale parked entry was evicted on timeout (no leak).
        assert_eq!(gate.pending_count().await, 0);
        assert!(matches!(
            events.events().await.as_slice(),
            [ClientEvent::PermissionRequestResolved {
                resolution: PermissionResolutionDto::Expired,
                ..
            }]
        ));
    }

    /// Finding 14: `ParkedRequestGuard` used to be disarmed unconditionally
    /// right after `tokio::time::timeout(...)` resolves — BEFORE the
    /// `Err(_elapsed)` arm's own `self.pending.lock().await.remove(...)`
    /// eviction ran. If the asker's future was dropped exactly while
    /// suspended on that (contended) lock acquire, the entry stayed parked
    /// forever: the guard was already disarmed (no backstop) and the
    /// in-future eviction that would have removed it never got to run
    /// (the future that contained it is gone). Reproduced deterministically
    /// by holding the `pending` map lock from outside the checking task
    /// across its timeout elapsing, then aborting it while it is blocked
    /// trying to acquire that same lock — the exact window described by
    /// finding 14 (a sibling parked/resolving request contending the map).
    #[tokio::test]
    async fn dropped_asker_after_timeout_evicts_parked_request_via_guard_backstop() {
        let sink = MockRequestSink::arc();
        let events = MockSink::arc();
        let gate = Arc::new(
            AdapterPermissionGate::new(sink.clone())
                .with_event_sink(events.clone())
                .with_timeout(Duration::from_millis(30)),
        );

        let g = gate.clone();
        let task = tokio::spawn(async move { g.check("Bash", &json!({"command": "ls"})).await });

        wait_for_pending(&gate, 1).await;
        let req = sink.last().await;

        // Hold the `pending` map lock from OUTSIDE the checking task so its
        // own timeout-eviction (`self.pending.lock().await.remove(...)`) is
        // forced to actually suspend on a genuinely contended lock, rather
        // than resolving on first poll.
        let held = gate.pending.lock().await;

        // Real wall-clock wait for the checking task's `tokio::time::timeout`
        // to elapse and for it to reach (and block on) that lock acquire.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // The asker vanishes exactly in that window — e.g. the panel-bar
        // `join_set.abort_all()` racing a parked ask whose deadline just
        // fired.
        task.abort();
        let _ = task.await;

        drop(held);

        for _ in 0..2000 {
            if gate.pending_count().await == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            gate.pending_count().await,
            0,
            "a parked request whose asker was dropped exactly as its own timeout eviction \
             was about to run must not be leaked forever"
        );

        for _ in 0..2000 {
            if !events.events().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let seen = events.events().await;
        assert!(
            matches!(
                seen.as_slice(),
                [ClientEvent::PermissionRequestResolved {
                    request_id,
                    resolution: PermissionResolutionDto::Cancelled,
                }] if *request_id == req.request_id
            ),
            "expected exactly one Cancelled resolution for request {}; got: {seen:?}",
            req.request_id
        );
    }

    /// `tool_use_confirm_constructed_with_default_allow` — the `tool_default` →
    /// `default_allow: bool` collapse: `Read` is allow-by-default, `Bash` is not.
    #[tokio::test]
    async fn tool_use_confirm_constructed_with_default_allow() {
        let sink = MockRequestSink::arc();
        let gate = Arc::new(AdapterPermissionGate::new(sink.clone()));

        // Read → AllowByDefault → default_allow: true.
        let g = gate.clone();
        let t = tokio::spawn(async move { g.check("Read", &json!({"path": "/tmp/x"})).await });
        wait_for_pending(&gate, 1).await;
        let read_req = sink.last().await;
        match &read_req.kind {
            PermissionKindDto::ToolUseConfirm {
                tool_name,
                tool_input_json,
                default_allow,
            } => {
                assert_eq!(tool_name, "Read");
                assert!(*default_allow, "Read is allow-by-default");
                // Input was lowered to a JSON string (decision §0.4).
                assert_eq!(tool_input_json, &json!({"path": "/tmp/x"}).to_string());
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        assert!(
            gate.resolve(
                read_req.request_id,
                PermissionResponseDto::AllowOnce,
                "Read"
            )
            .await
        );
        t.await.unwrap();

        // Bash → DenyByDefault → default_allow: false.
        let g = gate.clone();
        let t = tokio::spawn(async move { g.check("Bash", &json!({})).await });
        wait_for_pending(&gate, 1).await;
        let bash_req = sink.last().await;
        match &bash_req.kind {
            PermissionKindDto::ToolUseConfirm { default_allow, .. } => {
                assert!(!*default_allow, "Bash is deny-by-default");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        assert!(
            gate.resolve(bash_req.request_id, PermissionResponseDto::Deny, "Bash")
                .await
        );
        t.await.unwrap();
    }

    /// Resolving an unknown / already-resolved id is a safe no-op.
    #[tokio::test]
    async fn resolve_unknown_id_is_noop() {
        let sink = MockRequestSink::arc();
        let gate = AdapterPermissionGate::new(sink);
        assert!(
            !gate
                .resolve(999, PermissionResponseDto::AllowOnce, "Bash")
                .await
        );
    }
}
