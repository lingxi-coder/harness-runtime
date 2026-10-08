//! Bidirectional control-plane state for the stream-json `--input-format
//! stream-json` path (P5 Phase 2+).
//!
//! Mirrors `StructuredIO.pendingRequests` + `sendRequest`/`processLine` from
//! claude-code's `cli/structuredIO.ts`. Holds:
//!  - the shared single-writer outbound channel (so a CLI-originated
//!    `control_request` never overtakes a queued data frame — the
//!    structuredIO.ts:160-162 design lock),
//!  - the `pendingRequests` map keyed by `request_id` (for the `can_use_tool`
//!    permission round-trip — the ONE subtype the CLI originates),
//!  - the resolved-tool-use-id dedup ring (`MAX_RESOLVED_TOOL_USE_IDS`,
//!    oldest-evicted — duplicate-response drop is wired in Phase 4),
//!  - the active turn's `CancellationToken`, so a `deny+interrupt` permission
//!    response can abort the whole turn (§3.4).
//!
//! The plane is created in the CLI BEFORE `build_runtime` (its outbound handle
//! comes from `StreamJsonStream::outbound_tx()`, available pre-build) and shared
//! with the injected [`StdioControlPermissionGate`] and the run loop's resolver.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use permission::gate::{
    PermissionCheckContext, PermissionDecision, PermissionGate, PermissionOutcome, PromptWorker,
};
use permission::{
    persist_permission_mode, persist_permission_rule_set, persist_workspace_directories,
    replace_permission_rules, PermissionBehavior, PermissionPaths, PermissionRule,
    PermissionRuleSource, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
};

use crate::headless::permission_prompt_notify::{
    PermissionPromptNotifier, PermissionPromptNotifyGuard,
};
use crate::headless::stream_json::{serialize_ndjson_line, OutboundMsg, OutboundTx};

/// Cap on the resolved-tool-use dedup ring (claude-code
/// `MAX_RESOLVED_TOOL_USE_IDS`, oldest-evicted).
const MAX_RESOLVED_TOOL_USE_IDS: usize = 1000;

/// A registered outbound `control_request` awaiting its `control_response`.
struct PendingControlRequest {
    /// Resolved by [`StdioControlPlane::resolve_response`] with the inner
    /// `response.response` payload (`Ok`) or the `response.error` string (`Err`).
    responder: oneshot::Sender<Result<Utf16JsonProjection, String>>,
    /// The `tool_use_id` this request carried, tracked on resolve so a late /
    /// duplicate response can be deduped (§1.5).
    tool_use_id: Option<String>,
    envelope: Utf16JsonProjection,
    sequence: u64,
}

/// Typed pending-request broker. Admission and retirement hold this mutex
/// only for synchronous state/channel operations; bounded output writes are
/// drained separately and never run while the pending map is locked.
#[derive(Default)]
struct PendingRequestBroker {
    pending: Mutex<HashMap<String, PendingControlRequest>>,
    closed: CancellationToken,
    sequence: std::sync::atomic::AtomicU64,
}

impl PendingRequestBroker {
    async fn admit(
        &self,
        request_id: String,
        mut request: PendingControlRequest,
        line: String,
        outbound: &OutboundTx,
    ) {
        let mut pending = self.pending.lock().await;
        if self.closed.is_cancelled() {
            let _ = request.responder.send(Err(
                "Tool permission stream closed before response received".into(),
            ));
            return;
        }
        request.sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pending.insert(request_id.clone(), request);
        // Admission and enqueue are synchronous under one fence. The output
        // drain performs bounded/awaited writes after this lock is released.
        if outbound.send(OutboundMsg::Line(line)).is_err() {
            if let Some(request) = pending.remove(&request_id) {
                let _ = request
                    .responder
                    .send(Err("Control request output closed before delivery".into()));
            }
        }
    }

    async fn remove(&self, request_id: &str) -> Option<PendingControlRequest> {
        self.pending.lock().await.remove(request_id)
    }

    async fn ui_answer_is_unsettled(&self, request_id: &str, response: &Value) -> bool {
        let pending = self.pending.lock().await;
        let Some(subtype) = pending
            .get(request_id)
            .and_then(|entry| entry.envelope.value.pointer("/request/subtype"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        lingxi_core::host::ModRemoteUiAnswer::parse(subtype).is_some()
            && !super::remote_ui::valid_ui_answer(subtype, response)
    }

    async fn drain(&self) -> Vec<PendingControlRequest> {
        self.pending
            .lock()
            .await
            .drain()
            .map(|(_, entry)| entry)
            .collect()
    }

    async fn snapshots(&self, subtype: &str) -> Vec<Utf16JsonProjection> {
        let pending = self.pending.lock().await;
        let mut snapshots: Vec<_> = pending
            .values()
            .filter(|entry| {
                entry
                    .envelope
                    .value
                    .pointer("/request/subtype")
                    .and_then(Value::as_str)
                    == Some(subtype)
            })
            .map(|entry| (entry.sequence, entry.envelope.clone()))
            .collect();
        snapshots.sort_by_key(|(sequence, _)| *sequence);
        snapshots
            .into_iter()
            .map(|(_, projection)| projection)
            .collect()
    }

    fn close(&self) {
        self.closed.cancel();
    }
}

/// An unmatched permission decision retains its exact JSON projection until
/// orphan recovery applies the rewritten tool input.
#[derive(Debug)]
pub struct OrphanedPermission {
    pub tool_use_id: lingxi_core::types::ToolUseId,
    pub permission_decision: Utf16JsonProjection,
}

/// A host-origin render remains owned after cancellation; only its response
/// is suppressed. Identity and generation retirement share a synchronous lock,
/// matching the native request AbortController map without emitting a cancel.
struct InboundUiRequest {
    cancelled: CancellationToken,
}

pub(crate) struct InboundUiRequestGuard {
    pending: Arc<std::sync::Mutex<HashMap<String, Arc<InboundUiRequest>>>>,
    key: String,
    entry: Arc<InboundUiRequest>,
}

impl InboundUiRequestGuard {
    pub(crate) fn reply_if_live(
        &self,
        reply: impl FnOnce() -> Result<(), crate::headless::stream_json_input::InputError>,
    ) -> Result<(), crate::headless::stream_json_input::InputError> {
        // The enqueue is synchronous and unbounded. Keep cancellation and the
        // decision to enqueue atomic; never await while holding this lock.
        let _pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.entry.cancelled.is_cancelled() {
            Ok(())
        } else {
            reply()
        }
    }
}

impl Drop for InboundUiRequestGuard {
    fn drop(&mut self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            pending.remove(&self.key);
        }
    }
}

/// Shared bidirectional control-plane state (see module docs).
pub struct StdioControlPlane {
    /// The single-writer outbound NDJSON channel shared with `StreamJsonStream`
    /// (every producer enqueues here; one drain task is the sole stdout writer).
    outbound_tx: Arc<OutboundTx>,
    /// CLI-originated requests awaiting a response, keyed by `request_id`.
    broker: PendingRequestBroker,
    stopped: CancellationToken,
    inbound_ui_requests: Arc<std::sync::Mutex<HashMap<String, Arc<InboundUiRequest>>>>,
    auxiliary_tasks: std::sync::OnceLock<std::sync::Weak<crate::headless::run::PrintAuxTaskGroup>>,
    operation_gate: Arc<Mutex<()>>,
    /// Resolved tool_use_ids, for duplicate-response dedup (Phase 4).
    resolved_tool_use_ids: Mutex<VecDeque<String>>,
    /// The in-flight turn's cancellation token (for `deny+interrupt`).
    active_turn_cancel: Mutex<Option<CancellationToken>>,
    /// Optional sink for ORPHANED permission recoveries. When a late
    /// `control_response` cannot be matched to a pending request (process restart
    /// with `--resume`, or a duplicate/late delivery), the genuine-orphan tail of
    /// [`Self::resolve_response`] forwards an `OrphanedPermission` command here
    /// instead of dropping it — the run loop dequeues it and re-runs the tool via
    /// `ConversationOrchestrator::run_orphaned_permission`. `None` (the default)
    /// keeps the legacy warn+drop, so every existing caller of [`Self::new`] is
    /// unaffected. Twin of claude-code's `setUnexpectedResponseCallback` →
    /// `enqueue({mode:'orphaned-permission'})` (print.ts:2767, 5291).
    orphan_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<OrphanedPermission>>>,
    /// GATE-SYSMSG-01: the session-id handle SHARED with `StreamJsonStream` (set
    /// once via [`Self::set_session_id`] at boot), read to stamp the `session_id`
    /// on an emitted `permission_denied` system message. Unset ⇒ the frame omits
    /// `session_id` (only in tests that never wire a stream).
    session_id: std::sync::OnceLock<Arc<Mutex<String>>>,
    sdk_mcp_transport:
        std::sync::OnceLock<std::sync::Weak<crate::headless::sdk_mcp_transport::SdkMcpTransport>>,
}

impl StdioControlPlane {
    /// Build the plane over the stream's shared outbound channel.
    #[must_use]
    pub fn new(outbound_tx: Arc<OutboundTx>) -> Arc<Self> {
        Arc::new(Self {
            outbound_tx,
            broker: PendingRequestBroker::default(),
            stopped: CancellationToken::new(),
            inbound_ui_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            auxiliary_tasks: std::sync::OnceLock::new(),
            operation_gate: Arc::new(Mutex::new(())),
            resolved_tool_use_ids: Mutex::new(VecDeque::new()),
            active_turn_cancel: Mutex::new(None),
            orphan_tx: Mutex::new(None),
            session_id: std::sync::OnceLock::new(),
            sdk_mcp_transport: std::sync::OnceLock::new(),
        })
    }

    pub(crate) fn set_auxiliary_tasks(
        &self,
        owner: std::sync::Weak<crate::headless::run::PrintAuxTaskGroup>,
    ) {
        let _ = self.auxiliary_tasks.set(owner);
    }

    pub(crate) fn auxiliary_tasks(&self) -> Option<Arc<crate::headless::run::PrintAuxTaskGroup>> {
        self.auxiliary_tasks
            .get()
            .and_then(std::sync::Weak::upgrade)
    }

    pub(crate) fn outbound_writer(&self) -> crate::headless::stream_json_input::ControlPlaneWriter {
        crate::headless::stream_json_input::ControlPlaneWriter::new(self.outbound_tx.clone())
    }

    pub(crate) fn begin_inbound_ui_request(
        &self,
        request_id: &Utf16JsonProjection,
    ) -> Result<InboundUiRequestGuard, crate::headless::stream_json_input::InputError> {
        let key = request_id
            .to_json_string()
            .map_err(|_| crate::headless::stream_json_input::InputError::MalformedJson)?;
        let entry = Arc::new(InboundUiRequest {
            cancelled: self.stopped.child_token(),
        });
        let mut pending = self
            .inbound_ui_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = pending.insert(key.clone(), entry.clone()) {
            previous.cancelled.cancel();
        }
        Ok(InboundUiRequestGuard {
            pending: self.inbound_ui_requests.clone(),
            key,
            entry,
        })
    }

    pub(crate) fn cancel_inbound_request(&self, request_id: &Utf16JsonProjection) {
        let Ok(key) = request_id.to_json_string() else {
            return;
        };
        let pending = self
            .inbound_ui_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = pending.get(&key) {
            entry.cancelled.cancel();
        }
    }

    pub fn set_sdk_mcp_transport(
        &self,
        transport: std::sync::Weak<crate::headless::sdk_mcp_transport::SdkMcpTransport>,
    ) {
        let _ = self.sdk_mcp_transport.set(transport);
    }
    pub fn sdk_mcp_transport(
        &self,
    ) -> Option<Arc<crate::headless::sdk_mcp_transport::SdkMcpTransport>> {
        self.sdk_mcp_transport
            .get()
            .and_then(std::sync::Weak::upgrade)
    }

    /// GATE-SYSMSG-01: wire the session-id handle shared with `StreamJsonStream`
    /// so an emitted `permission_denied` system message carries the same
    /// `session_id` as every data frame. Set once at boot (idempotent; later calls
    /// are ignored).
    pub fn set_session_id(&self, handle: Arc<Mutex<String>>) {
        let _ = self.session_id.set(handle);
    }

    /// GATE-SYSMSG-01: emit the `permission_denied` system message on the shared
    /// outbound NDJSON channel — 1:1 with claude-code `createCanUseTool`'s deny
    /// arm (`{type:"system", subtype:"permission_denied", tool_name, tool_use_id,
    /// agent_id, decision_reason_type, decision_reason, message, uuid,
    /// session_id}`). Optional fields (`tool_use_id`/`agent_id`/
    /// `decision_reason_type`/`decision_reason`) are OMITTED when absent, matching
    /// the oracle's `.optional()` shape.
    pub async fn emit_permission_denied(
        &self,
        tool_name: &str,
        tool_use_id: Option<&str>,
        agent_id: Option<&str>,
        decision_reason_type: Option<&str>,
        decision_reason: Option<&str>,
        message: &str,
    ) {
        let session_id = match self.session_id.get() {
            Some(handle) => handle.lock().await.clone(),
            None => String::new(),
        };
        // Build in the ORACLE key order (serde_json `preserve_order` is on, so
        // insertion order is the wire order): type, subtype, tool_name,
        // tool_use_id, agent_id, decision_reason_type, decision_reason, message,
        // uuid, session_id. Optional fields are inserted at their position and
        // omitted entirely when absent (never null).
        let mut map = serde_json::Map::new();
        map.insert("type".into(), json!("system"));
        map.insert("subtype".into(), json!("permission_denied"));
        map.insert("tool_name".into(), json!(tool_name));
        if let Some(id) = tool_use_id {
            map.insert("tool_use_id".into(), json!(id));
        }
        if let Some(agent) = agent_id {
            map.insert("agent_id".into(), json!(agent));
        }
        if let Some(rt) = decision_reason_type {
            map.insert("decision_reason_type".into(), json!(rt));
        }
        if let Some(reason) = decision_reason {
            map.insert("decision_reason".into(), json!(reason));
        }
        map.insert("message".into(), json!(message));
        map.insert("uuid".into(), json!(Uuid::new_v4().to_string()));
        map.insert("session_id".into(), json!(session_id));
        let _ = self
            .outbound_tx
            .send(OutboundMsg::Line(serialize_ndjson_line(&Value::Object(
                map,
            ))));
    }

    /// Wire the orphaned-permission recovery sink (the run loop's mpsc receiver
    /// end lives in `run_stream_json_input_loop`). Called once after the session
    /// id is known but before the resolver task spawns, so a genuine orphan
    /// `control_response` is forwarded for recovery instead of dropped. Idempotent
    /// (last writer wins); a `None` sink leaves the legacy warn+drop behaviour.
    pub async fn set_orphan_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<OrphanedPermission>,
    ) {
        *self.orphan_tx.lock().await = Some(tx);
    }

    /// Register the current turn's cancellation token so a `deny+interrupt`
    /// permission response can abort the whole turn (§3.4). Called by the turn
    /// loop before each turn.
    pub async fn set_active_turn(&self, token: CancellationToken) {
        let mut active = self.active_turn_cancel.lock().await;
        if self.stopped.is_cancelled() {
            token.cancel();
        }
        *active = Some(token);
    }

    /// Serialize cwd relocation with actual operation ownership. The busy
    /// token is a cancellation handle; this lease closes check/start races.
    pub async fn lock_operation(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.operation_gate.clone().lock_owned().await
    }
    pub fn try_lock_operation(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.operation_gate.clone().try_lock_owned().ok()
    }

    /// Whether a turn is currently in flight.
    ///
    /// The turn loop registers its cancellation token before each turn and
    /// clears it after, so the token's presence IS the busy signal. Read by
    /// `set_cwd`, which must not move the session's working directory out from
    /// under a turn that is already resolving paths against it.
    pub async fn is_busy(&self) -> bool {
        self.active_turn_cancel.lock().await.is_some()
    }

    /// Clear the active-turn token between turns.
    pub async fn clear_active_turn(&self) {
        *self.active_turn_cancel.lock().await = None;
    }

    /// Cancel the active turn (the `deny+interrupt` / control-channel path).
    /// Returns whether a live owner existed. An interrupt received while idle
    /// must not be remembered and applied to the next turn.
    pub async fn cancel_active_turn(&self) -> bool {
        let token = self.active_turn_cancel.lock().await.clone();
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// A clone of the active turn's cancellation token, if any. The gate selects
    /// on it so a turn abort (interrupt / Ctrl-C) cancels an in-flight
    /// `can_use_tool` request rather than blocking forever.
    async fn active_turn_token(&self) -> Option<CancellationToken> {
        self.active_turn_cancel.lock().await.clone()
    }

    /// Abort an outbound `control_request` whose turn was cancelled / superseded
    /// (§1.4 outbound-emit): enqueue `control_cancel_request` to tell the host to
    /// drop its prompt, remove the pending entry locally (tracking its tool_use_id
    /// so a late response is deduped), and let the awaiting future fall through.
    pub async fn cancel_request(&self, request_id: &Utf16JsonProjection) {
        let identity = request_id.value.as_str().unwrap_or_default();
        let entry = self.broker.remove(identity).await;
        if let Some(tuid) = entry.and_then(|e| e.tool_use_id) {
            self.track_resolved(tuid).await;
        }
        let mut frame = Utf16JsonProjection::plain(json!({"type":"control_cancel_request"}));
        frame
            .set_field("request_id", request_id.clone())
            .expect("valid request identity projection");
        let line = crate::headless::stream_json::serialize_projected_ndjson_line(&frame)
            .expect("valid cancellation projection");
        let _ = self.outbound_tx.send(OutboundMsg::Line(line));
    }

    /// Emit a CLI-originated `control_request` and return a receiver resolved
    /// when the matching `control_response` arrives (`sendRequest`,
    /// structuredIO.ts:469). Mints a `request_id`, enqueues the frame on the
    /// shared outbound channel (FIFO — no overtaking data frames), and registers
    /// the pending entry.
    pub async fn send_request(
        &self,
        request: Utf16JsonProjection,
        tool_use_id: Option<String>,
    ) -> (
        String,
        oneshot::Receiver<Result<Utf16JsonProjection, String>>,
    ) {
        let request_id = Uuid::new_v4().to_string();
        let mut frame = Utf16JsonProjection::plain(json!({
            "type": "control_request",
            "request_id": request_id,
        }));
        // Preserve arbitrary host/tool payload strings and property names.
        frame
            .set_field("request", request)
            .expect("valid control request projection");
        let (tx, rx) = oneshot::channel();
        self.broker
            .admit(
                request_id.clone(),
                PendingControlRequest {
                    responder: tx,
                    tool_use_id,
                    envelope: frame.clone(),
                    sequence: 0,
                },
                crate::headless::stream_json::serialize_projected_ndjson_line(&frame)
                    .expect("valid control request projection"),
                &self.outbound_tx,
            )
            .await;
        (request_id, rx)
    }

    /// Current process requests in native admission order for initialize and
    /// reinitialize prompt redelivery. Exact request payloads remain projected.
    pub async fn pending_permission_requests(&self) -> Vec<Utf16JsonProjection> {
        self.broker.snapshots("can_use_tool").await
    }
    pub async fn pending_user_dialog_requests(&self) -> Vec<Utf16JsonProjection> {
        self.broker.snapshots("request_user_dialog").await
    }

    /// Close permission input admission and reject every accepted waiter. The
    /// pending mutex serializes this fence with new request registration.
    pub async fn close_input(&self, reason: &str) {
        self.broker.close();
        self.fail_all_pending(reason).await;
    }

    /// Stop the connection's active operation without discarding its busy owner.
    /// The operation clears that owner only after its actual work has settled.
    pub async fn shutdown(&self, reason: &str) {
        self.stopped.cancel();
        self.cancel_active_turn().await;
        self.close_input(reason).await;
    }

    /// Resolve an inbound `control_response` against `pendingRequests`
    /// (`processLine`, structuredIO.ts:362). Frame keys are already
    /// snake_case-normalized by the router (`normalize_control_message_keys`),
    /// so the join key is the inner `response.request_id` and the payload is the
    /// inner `response.response` (the load-bearing double nesting).
    pub async fn resolve_response(&self, frame: &Utf16JsonProjection) {
        if self.stopped.is_cancelled() {
            return;
        }
        let Some(response) = frame.value.get("response") else {
            return;
        };
        let request_id = response
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if self
            .broker
            .ui_answer_is_unsettled(&request_id, response)
            .await
        {
            return;
        }
        let entry = self.broker.remove(&request_id).await;
        let Some(entry) = entry else {
            // No matching pending entry. Two cases (claude-code `processLine`
            // orphan path, structuredIO.ts:374-399):
            let tool_use_id = response
                .get("response")
                .and_then(|p| p.get("toolUseID"))
                .and_then(Value::as_str);
            if let Some(tuid) = tool_use_id {
                if self.is_resolved(tuid).await {
                    // §1.5 dedup: the payload's `toolUseID` was already resolved (a
                    // websocket-reconnect double-delivery) — drop so it can't
                    // double-resolve a tool_use (which would 400 on a non-unique id).
                    tracing::debug!(
                        "Ignoring duplicate control_response for already-resolved toolUseID={} request_id={}",
                        tuid,
                        request_id
                    );
                    return;
                }
            }
            // GENUINE orphan: no pending request AND the toolUseID (if any) is not
            // a known-resolved duplicate. claude-code's `unexpectedResponseCallback`
            // (`handleOrphanedPermissionResponse`, print.ts:5241) recovers by
            // re-running the unresolved tool_use from the transcript. When a
            // recovery sink is wired (the stdio run loop), forward an
            // `OrphanedPermission` command carrying the toolUseID + the raw inner
            // permission payload; the loop looks the tool_use up in the (resumed)
            // session history and re-runs it (`run_orphaned_permission`). This is
            // the enqueue half of claude-code's `enqueue({mode:
            // 'orphaned-permission', orphanedPermission:{permissionResult,…}})`
            // (print.ts:5291). Without a sink, keep the legacy warn+drop.
            let orphan_tx = self.orphan_tx.lock().await.clone();
            if let (Some(tx), Some(tuid)) = (orphan_tx, tool_use_id) {
                let permission_decision = frame
                    .subprojection("/response/response")
                    .unwrap_or_else(|_| Utf16JsonProjection::plain(json!({})));
                let cmd = OrphanedPermission {
                    tool_use_id: lingxi_core::types::ToolUseId::from(tuid),
                    permission_decision,
                };
                if tx.send(cmd).is_err() {
                    tracing::warn!(
                        "Orphan recovery sink closed; dropping orphan control_response request_id={request_id} toolUseID={tuid}"
                    );
                }
                return;
            }
            // No recovery sink wired (non-stdio paths / tests) — surface the orphan
            // (warn) rather than dropping it silently so it stays observable.
            tracing::warn!(
                "Dropping orphan control_response (no pending request; no recovery sink) request_id={} toolUseID={:?}",
                request_id,
                tool_use_id
            );
            return;
        };
        if let Some(tuid) = entry.tool_use_id.clone() {
            self.track_resolved(tuid).await;
        }
        let subtype = response
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("");
        let result = if subtype == "error" {
            Err(response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("control_response error")
                .to_string())
        } else {
            // SUCCESS: payload at `response.response`; absent ⇒ `{}` (the
            // `rn(gt)` no-payload case omits the key, parsed as empty).
            Ok(frame
                .subprojection("/response/response")
                .unwrap_or_else(|_| Utf16JsonProjection::plain(json!({}))))
        };
        let _ = entry.responder.send(result);
    }

    /// Mark a tool_use_id resolved (oldest-evicted ring, cap
    /// `MAX_RESOLVED_TOOL_USE_IDS`). The duplicate-drop check that consults this
    /// ring lands in Phase 4.
    async fn track_resolved(&self, tool_use_id: String) {
        let mut ring = self.resolved_tool_use_ids.lock().await;
        if ring.iter().any(|id| id == &tool_use_id) {
            return;
        }
        ring.push_back(tool_use_id);
        while ring.len() > MAX_RESOLVED_TOOL_USE_IDS {
            ring.pop_front();
        }
    }

    /// Whether `tool_use_id` is in the resolved ring (§1.5 dedup check).
    async fn is_resolved(&self, tool_use_id: &str) -> bool {
        self.resolved_tool_use_ids
            .lock()
            .await
            .iter()
            .any(|id| id == tool_use_id)
    }

    /// Reject EVERY in-flight pending `control_request` with `reason`.
    ///
    /// Called when stdin reaches EOF (the host closed the input stream): without
    /// this, a gate awaiting a `can_use_tool` `control_response` would block
    /// forever, hanging the tool call / turn. Mirrors claude-code's
    /// `StructuredIO.read()` close path, which sets `inputClosed` and rejects
    /// every `pendingRequests` entry with *"Tool permission stream closed before
    /// response received"*. Each drained entry's `tool_use_id` is tracked as
    /// resolved so a late re-delivery is still deduped.
    pub async fn fail_all_pending(&self, reason: &str) {
        let drained = self.broker.drain().await;
        for entry in drained {
            if let Some(tuid) = entry.tool_use_id.clone() {
                self.track_resolved(tuid).await;
            }
            let _ = entry.responder.send(Err(reason.to_string()));
        }
    }
}

/// Inner-transport `PermissionGate` that resolves an unresolved `Ask` by
/// emitting a `can_use_tool` control_request and awaiting the host's
/// `control_response` (§3).
///
/// Sits BEHIND `PolicyPermissionGate` (the outer local pre-check), substituted
/// via `cfg.injected_permission_gate`. Mirrors `StructuredIO.createCanUseTool`,
/// but the local allow/deny pre-gate already ran in the outer policy gate, so
/// only an `ask` reaches here. Deferred (per spec §3.5): the local
/// PermissionRequest-hook race, the `updatedInput` rewrite, and the sandbox-ask
/// piggyback.
pub struct StdioControlPermissionGate {
    plane: Arc<StdioControlPlane>,
    persistence_enabled: std::sync::atomic::AtomicBool,
    /// Settings-file roots used to PERSIST the host's `updatedPermissions` rule
    /// updates carried by a `can_use_tool` ALLOW response (claude-code
    /// `persistPermissionUpdates`). `None` ⇒ persistence is skipped (the gate
    /// still maps the allow; only the settings write is suppressed — e.g. in
    /// unit tests with no real settings tree). Wired via [`Self::with_persist`]
    /// from the CLI config (`cfg.lingxi_home` / `cfg.cwd`).
    persist_paths: Option<PermissionPaths>,
    /// SH-02: the seam that fires the delayed `permission_prompt` `Notification`
    /// hook (claude-code `Cou`, oracle 2.1.238 @ 307013328). Set ONCE by the
    /// composition root after the orchestrator exists — the gate itself is built
    /// before it, because the gate is injected INTO the runtime config. Unset ⇒
    /// [`PermissionPromptNotifyGuard::arm`] is inert and no timer is armed.
    prompt_notifier: std::sync::OnceLock<Arc<dyn PermissionPromptNotifier>>,
}

impl StdioControlPermissionGate {
    /// Build the decider over the shared control plane.
    #[must_use]
    pub fn new(plane: Arc<StdioControlPlane>) -> Self {
        Self {
            plane,
            persistence_enabled: std::sync::atomic::AtomicBool::new(true),
            persist_paths: None,
            prompt_notifier: std::sync::OnceLock::new(),
        }
    }

    /// SH-02 — attach the `permission_prompt` notifier once the orchestrator
    /// exists. Takes `&self` (not `self`) because the gate has already been
    /// moved into the runtime config as an `Arc<dyn PermissionGate>` by the time
    /// the orchestrator is available; the composition root keeps a typed clone
    /// of the same `Arc` and calls this on it.
    ///
    /// Idempotent: a second call is ignored, so the notifier can never be
    /// swapped mid-session.
    pub fn set_prompt_notifier(&self, notifier: Arc<dyn PermissionPromptNotifier>) {
        let _ = self.prompt_notifier.set(notifier);
    }

    /// Attach the settings-file roots so an ALLOW response's `updatedPermissions`
    /// rule updates are PERSISTED (mirrors `TuiPermissionGate::with_persist`):
    /// `lingxi_home` resolves `userSettings`, `cwd` resolves
    /// project/local settings. Without this the host's rule updates are parsed +
    /// dropped (no settings write).
    #[must_use]
    pub fn with_persist(mut self, paths: PermissionPaths) -> Self {
        self.persist_paths = Some(paths);
        self
    }

    /// Collapse a [`PermissionOutcome`] to the 2-valued [`PermissionDecision`]
    /// (dropping any `updated_input`) for the `check`/`check_with_worker` callers
    /// that cannot apply a rewrite.
    fn outcome_to_decision(outcome: PermissionOutcome) -> PermissionDecision {
        match outcome {
            PermissionOutcome::Allow { .. } | PermissionOutcome::AllowAuto { .. } => {
                PermissionDecision::Allow
            }
            PermissionOutcome::Deny { reason } => PermissionDecision::Deny { reason },
        }
    }

    /// Run the `can_use_tool` round-trip and map the host's decision to a
    /// [`PermissionOutcome`] (carrying any `updatedInput` rewrite).
    async fn decide_outcome(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        // §3.2 outbound request: emit `tool_name, input, tool_use_id` (+ agent_id
        // when a worker is present, + decision_reason / permission_suggestions /
        // blocked_path when supplied — claude-code `createCanUseTool` sends
        // `decision_reason: serializeDecisionReason(...)`,
        // `permission_suggestions: mainPermissionResult.suggestions`,
        // `blocked_path: mainPermissionResult.blockedPath`). Each is OMITTED when
        // absent (the oracle's `.optional()` fields). Use the REAL assistant
        // tool_use id when the dispatcher provides it (so the host can correlate +
        // dedup), else mint one.
        let tool_use_id = ctx
            .tool_use_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        // claude-code `KNe(t.name)` — the SAME display name feeds both the
        // request's `display_name` field and the SH-02 notification message.
        let display_name = permission_display_name(name);
        let mut request = json!({
            "subtype": "can_use_tool",
            "tool_name": name,
            // claude-code `createCanUseTool` sends `display_name:h1e(tool_name)`
            // UNCONDITIONALLY (a human-readable label the SDK host renders):
            // strip any MCP `__`-namespaced prefix segments, `_`→space, and
            // title-case each word initial (see `permission_display_name`).
            "display_name": display_name.clone(),
            "input": input,
            "tool_use_id": tool_use_id,
        });
        if let Some(w) = &ctx.worker {
            request["agent_id"] = json!(w.name);
        }
        if let Some(reason) = &ctx.decision_reason {
            request["decision_reason"] = json!(reason);
        }
        // GATE-WIRE-01: `decision_reason_type: decisionReason?.type` (the
        // DISCRIMINATED reason kind — `rule`/`mode`/`subcommandResults`/… — which
        // an SDK host parses for ask reasons where the `decision_reason` text is
        // `undefined`). Populated by the `PolicyPermissionGate` Ask path; OMITTED
        // when `None` (the LingXi-internal denial/auto-mode/bypass reasons carry
        // no CC `.type`).
        if let Some(rt) = &ctx.decision_reason_type {
            request["decision_reason_type"] = json!(rt);
        }
        if let Some(approvable) = ctx.classifier_approvable {
            request["classifier_approvable"] = json!(approvable);
        }
        if let Some(rule) = &ctx.matched_ask_rule {
            let mut wire_rule = json!({
                "source": rule.source,
                "tool_name": rule.tool_name,
            });
            if let Some(content) = &rule.rule_content {
                wire_rule["rule_content"] = json!(content);
            }
            request["matched_ask_rule"] = wire_rule;
        }
        // Upstream uses `requiresUserInteraction?.() || undefined`: false is
        // represented by absence, never by a literal false field.
        if ctx.requires_user_interaction {
            request["requires_user_interaction"] = json!(true);
        }
        // A requiresUserInteraction ask must not expose or honor a persistent
        // AllowAlways choice. Keep the key additive/optional for older hosts;
        // when supported, the exact oracle wire spelling is snake_case.
        if ctx.suppress_always_allow_rule {
            request["suppress_always_allow_rule"] = json!(true);
        }
        // Eligibility is computed by the engine and carried opaquely to the
        // host. Do not let a client infer an Auto action from tool name/input.
        if let Some(prompt) = ctx.auto_mode_prompt {
            request["auto_mode_prompt"] = json!(prompt.as_wire());
        }
        // Suggestions and blocked paths are emitted only when a structured
        // producer supplied them; never parse either out of display text.
        if let Some(suggestions) = &ctx.permission_suggestions {
            request["permission_suggestions"] = suggestions.clone();
        }
        if let Some(path) = &ctx.blocked_path {
            request["blocked_path"] = json!(path);
        }
        // SH-02 — `A=Cou(KNe(t.name))` (oracle @ 307031502): arm the 6 s timer
        // AROUND the round trip. Every exit from here — response, host error,
        // turn abort, dropped channel — drops `_notify_guard`, which is
        // upstream's `finally { A() }` / `clearTimeout`. A prompt answered
        // inside 6 s therefore fires no `Notification` hook at all.
        let _notify_guard =
            PermissionPromptNotifyGuard::arm(self.prompt_notifier.get(), &display_name);
        let input_projection = match ctx.projected_input(input) {
            Ok(projection) => projection,
            Err(error) => {
                return PermissionOutcome::Deny {
                    reason: format!(
                        "Tool permission request failed: invalid input projection: {error}"
                    ),
                }
            }
        };
        let mut request = Utf16JsonProjection::plain(request);
        request
            .set_field("input", input_projection)
            .expect("validated permission input projection");
        let (request_id, rx) = self.plane.send_request(request, Some(tool_use_id)).await;

        // There is NO timeout on the can_use_tool request (§3.1): block until a
        // control_response arrives, the channel drops, or the turn is aborted.
        // On a turn abort (interrupt / Ctrl-C), emit `control_cancel_request` so
        // the host drops its prompt, and deny (§1.4 / §3.1 AbortError → deny).
        let result = match self.plane.active_turn_token().await {
            Some(token) => {
                tokio::select! {
                    r = rx => r,
                    () = token.cancelled() => {
                        self.plane.cancel_request(&Utf16JsonProjection::plain(json!(request_id))).await;
                        // claude-code rejects a turn-aborted request with
                        // `new AbortError()`, so `String(error)` → "AbortError".
                        return PermissionOutcome::Deny {
                            reason: "Tool permission request failed: AbortError".to_string(),
                        };
                    }
                }
            }
            None => rx.await,
        };
        match result {
            Ok(Ok(payload)) => {
                let mut outcome = self.map_payload(payload).await;
                if matches!(&outcome, PermissionOutcome::AllowAuto { .. })
                    && (ctx.auto_mode_prompt.is_none() || ctx.suppress_always_allow_rule)
                {
                    // A stale/malicious host response cannot switch mode unless
                    // the engine explicitly marked this request eligible. Keep
                    // the current approval as a one-shot allow.
                    outcome = match outcome {
                        PermissionOutcome::AllowAuto { updated_input } => {
                            PermissionOutcome::Allow {
                                updated_input,
                                permission_updates: Vec::new(),
                                decision_classification: Some(
                                    lingxi_core::host::permission_gate::ToolDecisionClassification::UserTemporary,
                                ),
                            }
                        }
                        other => other,
                    };
                }
                if ctx.suppress_always_allow_rule {
                    // A stale or malicious host can still return an
                    // AllowAlways-shaped response (`user_permanent` and/or
                    // updatedPermissions). Preserve the explicit approval for
                    // this invocation, but discard every rule update so no
                    // session/local persistence can be created.
                    if let PermissionOutcome::Allow {
                        permission_updates,
                        decision_classification,
                        ..
                    } = &mut outcome
                    {
                        permission_updates.clear();
                        if matches!(
                            decision_classification,
                            Some(lingxi_core::host::permission_gate::ToolDecisionClassification::UserPermanent)
                        ) {
                            *decision_classification = Some(
                                lingxi_core::host::permission_gate::ToolDecisionClassification::UserTemporary,
                            );
                        }
                    }
                }
                // §2b: persist every file-backed `updatedPermissions` union
                // member here. The outer PolicyPermissionGate applies the same
                // array to live state; persistence remains best-effort and can
                // never turn the host's allow into a deny.
                if let PermissionOutcome::Allow {
                    permission_updates, ..
                } = &outcome
                {
                    if !permission_updates.is_empty() {
                        self.persist_permission_updates_to_disk(permission_updates)
                            .await;
                    }
                }
                outcome
            }
            Ok(Err(err)) => PermissionOutcome::Deny {
                reason: format!("Tool permission request failed: {err}"),
            },
            Err(_) => PermissionOutcome::Deny {
                // The oneshot dropped without a value (the plane was torn down):
                // the same closed-stream condition the binary rejects with
                // Error("Tool permission stream closed before response received").
                reason: "Tool permission request failed: Tool permission stream closed before response received"
                    .to_string(),
            },
        }
    }

    /// Parse + PERSIST the host's `updatedPermissions` rule updates from a
    /// `can_use_tool` ALLOW response (claude-code `persistPermissionUpdates`).
    ///
    /// Best-effort and fire-and-forget — 1:1 with the oracle, where
    /// `persistPermissionUpdates` is called without awaiting / surfacing errors:
    /// a parse skip or a [`permission::PersistError`] is logged and the allow
    /// still stands. Every file-backed update variant is persisted; session and
    /// CLI destinations remain live-only. A malformed entry is skipped without
    /// partially applying its valid-looking members.
    ///
    /// Live application is owned by the outer `PolicyPermissionGate`; this
    /// transport owns only the settings paths and therefore the durable half.
    async fn persist_permission_updates_to_disk(&self, updates: &[Value]) {
        if !self
            .persistence_enabled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let Some(paths) = &self.persist_paths else {
            // No settings tree wired (e.g. unit tests) — parse-only, no write.
            return;
        };
        for raw in updates {
            let Some(update) = parse_persistent_permission_update(raw) else {
                tracing::debug!("Skipping malformed permission update from can_use_tool response");
                continue;
            };
            if let Err(e) = persist_parsed_permission_update(update, paths).await {
                tracing::warn!(
                    "Failed to persist permission update from can_use_tool response: {e}"
                );
            }
        }
    }

    /// Map a `PermissionToolOutput` payload onto a [`PermissionOutcome`] (§3.3).
    async fn map_payload(&self, projection: Utf16JsonProjection) -> PermissionOutcome {
        let payload = &projection.value;
        match payload.get("behavior").and_then(Value::as_str) {
            Some("allow") => {
                // §3.3 / strict schema: an allow result REQUIRES an `updatedInput`
                // object key (claude-code `PermissionAllowResultSchema`); a missing
                // (or non-object) key is a malformed allow → deny. The value may be
                // `{}` (no rewrite); only a NON-EMPTY object substitutes the tool
                // input (claude-code applies `updatedInput` "when it has keys").
                match payload.get("updatedInput") {
                    Some(Value::Object(map)) => {
                        let updated_input = if map.is_empty() {
                            None
                        } else {
                            projection.subprojection("/updatedInput").ok()
                        };
                        // §2b / `applyPermissionUpdates` + `persistPermissionUpdates`:
                        // an allow may also carry an `updatedPermissions` array of
                        // permission-rule updates the host wants applied + persisted.
                        // It is OPTIONAL and independent of `updatedInput`: a missing
                        // or non-array value is simply no updates (mirroring the
                        // oracle's `.catch(undefined)` — a malformed array is IGNORED,
                        // never a deny). The raw wire array is carried through and
                        // parsed + persisted by the consumer ([`Self::decide_outcome`]).
                        let permission_updates = match payload.get("updatedPermissions") {
                            Some(Value::Array(arr)) => arr.clone(),
                            _ => Vec::new(),
                        };
                        let decision_classification = payload
                            .get("decisionClassification")
                            .and_then(Value::as_str)
                            .and_then(|value| match value {
                                "user_temporary" => Some(
                                    lingxi_core::host::permission_gate::ToolDecisionClassification::UserTemporary,
                                ),
                                "user_permanent" => Some(
                                    lingxi_core::host::permission_gate::ToolDecisionClassification::UserPermanent,
                                ),
                                "user_reject" => Some(
                                    lingxi_core::host::permission_gate::ToolDecisionClassification::UserReject,
                                ),
                                _ => None,
                            });
                        let auto_selected = payload
                            .get("permissionMode")
                            .or_else(|| payload.get("mode"))
                            .and_then(Value::as_str)
                            .is_some_and(|mode| mode == "auto");
                        if auto_selected {
                            PermissionOutcome::AllowAuto { updated_input }
                        } else {
                            PermissionOutcome::Allow {
                            updated_input,
                            permission_updates,
                            decision_classification,
                            }
                        }
                    }
                    _ => PermissionOutcome::Deny {
                        reason: "Tool permission request failed: malformed allow result (missing updatedInput)"
                            .to_string(),
                    },
                }
            }
            Some("allow_auto") => {
                let updated_input = payload
                    .get("updatedInput")
                    .and_then(Value::as_object)
                    .filter(|map| !map.is_empty())
                    .and_then(|_| projection.subprojection("/updatedInput").ok());
                PermissionOutcome::AllowAuto { updated_input }
            }
            Some("deny") => {
                // The oracle's deny schema REQUIRES `message`; a deny without it
                // is a Zod parse failure → the createCanUseTool catch denies with
                // the "Tool permission request failed: <error>" family, NOT a
                // hard-coded "Permission denied".
                let Some(message) = payload.get("message").and_then(Value::as_str) else {
                    return PermissionOutcome::Deny {
                        reason: "Tool permission request failed: malformed deny result (missing message)"
                            .to_string(),
                    };
                };
                let message = message.to_string();
                if payload
                    .get("interrupt")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    // deny+interrupt: abort the whole turn, not just this tool
                    // (§3.4 — the binary calls `ctx.abortController.abort()`).
                    self.plane.cancel_active_turn().await;
                }
                PermissionOutcome::Deny { reason: message }
            }
            // Any non-allow/deny behavior is a schema-invalid result; the oracle
            // funnels it through the same "Tool permission request failed: …"
            // catch (the exact suffix is a ZodError serialization we don't
            // reproduce — the byte-faithful part is the prefix).
            _ => PermissionOutcome::Deny {
                reason: "Tool permission request failed: invalid permission result".to_string(),
            },
        }
    }
}

#[derive(Debug)]
enum PersistentPermissionUpdate {
    AddRules(Vec<PermissionUpdate>),
    ReplaceRules {
        behavior: PermissionBehavior,
        rules: Vec<PermissionRule>,
        destination: PermissionUpdateDestination,
    },
    RemoveRules(Vec<PermissionUpdate>),
    SetMode {
        mode: String,
        destination: PermissionUpdateDestination,
    },
    Directories {
        directories: Vec<String>,
        add: bool,
        destination: PermissionUpdateDestination,
    },
}

/// Strictly parse one `permissionUpdateSchema` union member. A malformed rule
/// makes the whole member invalid so persistence cannot apply a partial prefix.
fn parse_persistent_permission_update(raw: &Value) -> Option<PersistentPermissionUpdate> {
    let obj = raw.as_object()?;
    let kind = obj.get("type").and_then(Value::as_str)?;
    let destination = obj
        .get("destination")
        .and_then(Value::as_str)
        .and_then(parse_destination)?;
    match kind {
        "addRules" | "replaceRules" | "removeRules" => {
            let behavior = obj
                .get("behavior")
                .and_then(Value::as_str)
                .and_then(parse_behavior)?;
            let source = source_for_destination(destination);
            let rules = obj
                .get("rules")
                .and_then(Value::as_array)?
                .iter()
                .map(|rule_value| {
                    let rule = rule_value.as_object()?;
                    let tool_name = rule.get("toolName").and_then(Value::as_str)?;
                    let rule_content = match rule.get("ruleContent") {
                        None => None,
                        Some(Value::String(content)) => Some(content.clone()),
                        Some(_) => return None,
                    };
                    Some(PermissionRule {
                        value: PermissionRuleValue {
                            tool_name: permission::rule::normalize_legacy_tool_name(tool_name),
                            rule_content,
                        },
                        behavior,
                        source,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            if kind == "replaceRules" {
                Some(PersistentPermissionUpdate::ReplaceRules {
                    behavior,
                    rules,
                    destination,
                })
            } else {
                let updates = rules
                    .into_iter()
                    .map(|rule| PermissionUpdate { rule, destination })
                    .collect();
                if kind == "addRules" {
                    Some(PersistentPermissionUpdate::AddRules(updates))
                } else {
                    Some(PersistentPermissionUpdate::RemoveRules(updates))
                }
            }
        }
        "setMode" => {
            let mode = obj.get("mode").and_then(Value::as_str)?;
            if !matches!(
                mode,
                "default" | "acceptEdits" | "bypassPermissions" | "plan" | "dontAsk" | "auto"
            ) {
                return None;
            }
            Some(PersistentPermissionUpdate::SetMode {
                mode: mode.to_string(),
                destination,
            })
        }
        "addDirectories" | "removeDirectories" => {
            let directories = obj
                .get("directories")
                .and_then(Value::as_array)?
                .iter()
                .map(Value::as_str)
                .map(|value| value.map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            // `persistPermissionUpdates` rejects the whole directory update
            // when any member contains NUL. Validate before handing the array
            // to the persistence layer so a mixed valid/NUL update cannot
            // create a partial settings write.
            if let Some(directory) = directories
                .iter()
                .find(|directory| directory.contains('\0'))
            {
                tracing::debug!("{}", directory_update_null_byte_reason(kind, directory));
                return None;
            }
            Some(PersistentPermissionUpdate::Directories {
                directories,
                add: kind == "addDirectories",
                destination,
            })
        }
        _ => None,
    }
}

fn directory_update_null_byte_reason(update_type: &str, directory: &str) -> String {
    format!("{update_type} carries a directory containing a null byte: {directory}")
}

async fn persist_parsed_permission_update(
    update: PersistentPermissionUpdate,
    paths: &PermissionPaths,
) -> Result<(), permission::PersistError> {
    match update {
        PersistentPermissionUpdate::AddRules(updates) => {
            if let Some(first) = updates.first() {
                let destination = first.destination;
                let rules: Vec<_> = updates.into_iter().map(|update| update.rule).collect();
                persist_permission_rule_set(&rules, true, destination, paths).await?;
            }
        }
        PersistentPermissionUpdate::ReplaceRules {
            behavior,
            rules,
            destination,
        } => {
            replace_permission_rules(behavior, &rules, destination, paths).await?;
        }
        PersistentPermissionUpdate::RemoveRules(updates) => {
            if let Some(first) = updates.first() {
                let destination = first.destination;
                let rules: Vec<_> = updates.into_iter().map(|update| update.rule).collect();
                persist_permission_rule_set(&rules, false, destination, paths).await?;
            }
        }
        PersistentPermissionUpdate::SetMode { mode, destination } => {
            persist_permission_mode(&mode, destination, paths).await?;
        }
        PersistentPermissionUpdate::Directories {
            directories,
            add,
            destination,
        } => {
            persist_workspace_directories(&directories, add, destination, paths).await?;
        }
    }
    Ok(())
}

/// Map the wire behavior string (`allow`/`deny`/`ask`) to a [`PermissionBehavior`].
fn parse_behavior(s: &str) -> Option<PermissionBehavior> {
    match s {
        "allow" => Some(PermissionBehavior::Allow),
        "deny" => Some(PermissionBehavior::Deny),
        "ask" => Some(PermissionBehavior::Ask),
        _ => None,
    }
}

/// Map the wire destination string to a [`PermissionUpdateDestination`]
/// (`permissionUpdateDestinationSchema`).
fn parse_destination(s: &str) -> Option<PermissionUpdateDestination> {
    match s {
        "userSettings" => Some(PermissionUpdateDestination::UserSettings),
        "projectSettings" => Some(PermissionUpdateDestination::ProjectSettings),
        "localSettings" => Some(PermissionUpdateDestination::LocalSettings),
        "session" => Some(PermissionUpdateDestination::Session),
        "cliArg" => Some(PermissionUpdateDestination::CliArg),
        _ => None,
    }
}

/// The [`PermissionRuleSource`] a rule persisted to `destination` should carry
/// (the file it lives in determines its precedence on the next load).
fn source_for_destination(dest: PermissionUpdateDestination) -> PermissionRuleSource {
    match dest {
        PermissionUpdateDestination::UserSettings => {
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User)
        }
        PermissionUpdateDestination::ProjectSettings => {
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        }
        PermissionUpdateDestination::LocalSettings => {
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local)
        }
        PermissionUpdateDestination::CliArg => PermissionRuleSource::CliArg,
        PermissionUpdateDestination::Session => PermissionRuleSource::Session,
    }
}

/// Port of claude-code `h1e(e)` — the `display_name` a `can_use_tool`
/// control_request carries for a tool.
///
/// ```js
/// function h1e(e){return(e.split("__").pop()||e).replace(/_/g," ").replace(/\b\w/g,(r)=>r.toUpperCase())}
/// ```
///
/// 1. Take the LAST `__`-delimited segment (strips an MCP `mcp__server__` prefix
///    so only the bare tool name remains); if that segment is empty (a trailing
///    `__`), fall back to the full name — matching JS `pop() || e`.
/// 2. Replace every `_` with a space.
/// 3. Title-case: uppercase the first word char of each word (JS `\b\w`).
fn permission_display_name(name: &str) -> String {
    let last = name.rsplit("__").next().unwrap_or(name);
    let last = if last.is_empty() { name } else { last };
    let spaced = last.replace('_', " ");
    let mut out = String::with_capacity(spaced.len());
    // JS `\w` is ASCII [A-Za-z0-9_]; after the `_`→space pass only alphanumerics
    // remain as word chars. Uppercase the first word char after any boundary.
    let mut prev_is_word = false;
    for ch in spaced.chars() {
        let is_word = ch.is_ascii_alphanumeric() || ch == '_';
        if is_word && !prev_is_word {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push(ch);
        }
        prev_is_word = is_word;
    }
    out
}

#[async_trait]
impl PermissionGate for StdioControlPermissionGate {
    fn set_permission_persistence_enabled(&self, enabled: bool) {
        self.persistence_enabled
            .store(enabled, std::sync::atomic::Ordering::Release);
    }

    async fn check(&self, name: &str, input: &Value) -> PermissionDecision {
        Self::outcome_to_decision(
            self.decide_outcome(name, input, &PermissionCheckContext::default())
                .await,
        )
    }

    async fn check_with_worker(
        &self,
        name: &str,
        input: &Value,
        worker: Option<PromptWorker>,
    ) -> PermissionDecision {
        let ctx = PermissionCheckContext {
            worker,
            ..PermissionCheckContext::default()
        };
        Self::outcome_to_decision(self.decide_outcome(name, input, &ctx).await)
    }

    async fn check_with_context(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.decide_outcome(name, input, ctx).await
    }

    async fn persist_permission_updates(&self, updates: &[Value]) {
        self.persist_permission_updates_to_disk(updates).await;
    }

    /// GATE-SYSMSG-01: emit the `permission_denied` system message when the OUTER
    /// `PolicyPermissionGate` locally denies a tool. Mirrors `createCanUseTool`'s
    /// deny arm; the `agent_id` comes from the worker context exactly like the
    /// `can_use_tool` request.
    async fn on_permission_denied(
        &self,
        name: &str,
        ctx: &PermissionCheckContext,
        decision_reason_type: Option<&str>,
        decision_reason: Option<&str>,
        message: &str,
    ) {
        self.plane
            .emit_permission_denied(
                name,
                ctx.tool_use_id.as_deref(),
                ctx.worker.as_ref().map(|w| w.name.as_str()),
                decision_reason_type,
                decision_reason,
                message,
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use permission::gate::MatchedAskRule;
    use tokio::sync::mpsc;

    /// Build a plane wired to an in-memory outbound channel; the receiver lets a
    /// test read the frames the plane emits (simulating the host on stdin).
    fn plane_with_channel() -> (Arc<StdioControlPlane>, mpsc::UnboundedReceiver<OutboundMsg>) {
        let (tx, rx) = mpsc::unbounded_channel::<OutboundMsg>();
        (StdioControlPlane::new(Arc::new(tx)), rx)
    }

    fn outbound_line(msg: OutboundMsg) -> String {
        match msg {
            OutboundMsg::Line(line) => line,
            OutboundMsg::StreamEvent(line) => line,
            OutboundMsg::Heartbeats(_) => panic!("unexpected heartbeat message"),
            OutboundMsg::Flush(_)
            | OutboundMsg::Shutdown(_)
            | OutboundMsg::PublishJson(_)
            | OutboundMsg::RefreshHeldResultTotals(_) => {
                panic!("unexpected output barrier")
            }
        }
    }

    #[test]
    fn inbound_render_cancel_retains_owner_and_exact_request_identity() {
        let (plane, mut rx) = plane_with_channel();
        let first_id = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
        let other_id = Utf16JsonProjection::parse(r#""\ud801""#).unwrap();
        let first = plane.begin_inbound_ui_request(&first_id).unwrap();
        let other = plane.begin_inbound_ui_request(&other_id).unwrap();
        plane.cancel_inbound_request(&first_id);
        assert_eq!(
            plane.inbound_ui_requests.lock().unwrap().len(),
            2,
            "cancellation does not retire the owner"
        );
        first
            .reply_if_live(|| panic!("cancelled render must not reply"))
            .unwrap();
        let delivered = std::sync::atomic::AtomicBool::new(false);
        other
            .reply_if_live(|| {
                delivered.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        assert!(delivered.load(std::sync::atomic::Ordering::Relaxed));
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "inbound cancellation never echoes a wire cancel"
        );
        drop(first);
        drop(other);
        assert!(plane.inbound_ui_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn replaced_render_owner_cannot_retire_or_reply_for_new_generation() {
        let (plane, _) = plane_with_channel();
        let id = Utf16JsonProjection::plain(json!("render"));
        let old = plane.begin_inbound_ui_request(&id).unwrap();
        let current = plane.begin_inbound_ui_request(&id).unwrap();
        old.reply_if_live(|| panic!("replaced render must not reply"))
            .unwrap();
        drop(old);
        assert_eq!(plane.inbound_ui_requests.lock().unwrap().len(), 1);
        current.reply_if_live(|| Ok(())).unwrap();
        drop(current);
        assert!(plane.inbound_ui_requests.lock().unwrap().is_empty());
    }

    fn success_response(request_id: &str, payload: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::plain(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": payload,
            }
        }))
    }

    // ── GATE-SYSMSG-01: permission_denied system message ──

    #[tokio::test]
    async fn emit_permission_denied_full_frame_shape() {
        let (plane, mut rx) = plane_with_channel();
        let sid = Arc::new(Mutex::new("sess-123".to_string()));
        plane.set_session_id(sid);

        plane
            .emit_permission_denied(
                "Bash",
                Some("tu-9"),
                Some("agent-A"),
                Some("safetyCheck"),
                Some("dangerous rm"),
                "Permission denied: rm",
            )
            .await;

        let line = outbound_line(rx.recv().await.unwrap());
        // Key ORDER must match the oracle (serde_json preserve_order → wire order).
        let map: serde_json::Map<String, Value> = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "type",
                "subtype",
                "tool_name",
                "tool_use_id",
                "agent_id",
                "decision_reason_type",
                "decision_reason",
                "message",
                "uuid",
                "session_id",
            ]
        );
        let f: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(f["type"], "system");
        assert_eq!(f["subtype"], "permission_denied");
        assert_eq!(f["tool_name"], "Bash");
        assert_eq!(f["tool_use_id"], "tu-9");
        assert_eq!(f["agent_id"], "agent-A");
        assert_eq!(f["decision_reason_type"], "safetyCheck");
        assert_eq!(f["decision_reason"], "dangerous rm");
        assert_eq!(f["message"], "Permission denied: rm");
        assert_eq!(f["session_id"], "sess-123");
        assert!(f["uuid"].as_str().is_some_and(|u| !u.is_empty()));
    }

    #[tokio::test]
    async fn emit_permission_denied_omits_absent_optionals() {
        let (plane, mut rx) = plane_with_channel();
        // No session id wired, no optionals — the oracle `.optional()` fields are
        // OMITTED (never emitted as null).
        plane
            .emit_permission_denied("Read", None, None, None, None, "denied")
            .await;
        let f: Value = serde_json::from_str(&outbound_line(rx.recv().await.unwrap())).unwrap();
        assert_eq!(f["subtype"], "permission_denied");
        assert_eq!(f["tool_name"], "Read");
        assert_eq!(f["message"], "denied");
        assert!(f.get("tool_use_id").is_none());
        assert!(f.get("agent_id").is_none());
        assert!(f.get("decision_reason_type").is_none());
        assert!(f.get("decision_reason").is_none());
        // session_id is present-but-empty when no stream handle is wired.
        assert_eq!(f["session_id"], "");
    }

    #[tokio::test]
    async fn permission_denied_session_id_reflects_late_set() {
        let (plane, mut rx) = plane_with_channel();
        // Wire an EMPTY handle (as at boot), then let the "stream" set the real id
        // afterwards through the SAME shared Mutex.
        let sid = Arc::new(Mutex::new(String::new()));
        plane.set_session_id(sid.clone());
        *sid.lock().await = "sess-late".to_string();
        plane
            .emit_permission_denied("Bash", None, None, None, None, "x")
            .await;
        let f: Value = serde_json::from_str(&outbound_line(rx.recv().await.unwrap())).unwrap();
        assert_eq!(f["session_id"], "sess-late");
    }

    #[tokio::test]
    async fn send_request_emits_frame_and_resolves_success() {
        let (plane, mut rx) = plane_with_channel();
        let (req_id, fut) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype": "can_use_tool", "tool_name": "Bash"})),
                Some("tu1".to_string()),
            )
            .await;

        let line = outbound_line(rx.recv().await.expect("frame emitted"));
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "control_request");
        assert_eq!(frame["request"]["tool_name"], "Bash");
        assert_eq!(frame["request_id"].as_str().unwrap(), req_id);

        plane
            .resolve_response(&success_response(&req_id, json!({"behavior": "allow"})))
            .await;
        let payload = fut.await.unwrap().unwrap();
        assert_eq!(payload.value["behavior"], "allow");
    }

    #[tokio::test]
    async fn resolve_response_error_subtype_rejects() {
        let (plane, _rx) = plane_with_channel();
        let (req_id, fut) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype": "can_use_tool"})),
                None,
            )
            .await;

        let resp = json!({
            "type": "control_response",
            "response": {"subtype": "error", "request_id": req_id, "error": "boom"}
        });
        plane
            .resolve_response(&Utf16JsonProjection::plain(resp.clone()))
            .await;
        assert_eq!(fut.await.unwrap().unwrap_err(), "boom");
    }

    #[tokio::test]
    async fn success_with_no_inner_payload_resolves_empty_object() {
        let (plane, _rx) = plane_with_channel();
        let (req_id, fut) = plane
            .send_request(Utf16JsonProjection::plain(json!({"subtype": "x"})), None)
            .await;
        // `response` key absent (the `rn(gt)` no-payload case).
        let resp = json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": req_id}
        });
        plane
            .resolve_response(&Utf16JsonProjection::plain(resp.clone()))
            .await;
        assert_eq!(fut.await.unwrap().unwrap().value, json!({}));
    }

    #[tokio::test]
    async fn turn_abort_emits_control_cancel_request_and_denies() {
        // §1.4 / §3.1: a turn abort while the gate awaits cancels the outbound
        // request and denies.
        let (plane, mut rx) = plane_with_channel();
        let token = CancellationToken::new();
        plane.set_active_turn(token.clone()).await;
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });

        // Drain the can_use_tool request, then abort the turn.
        let line = outbound_line(rx.recv().await.unwrap());
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["request"]["subtype"],
            "can_use_tool"
        );
        token.cancel();

        assert_eq!(
            check.await.unwrap(),
            PermissionDecision::Deny {
                reason: "Tool permission request failed: AbortError".to_string()
            }
        );
        // A control_cancel_request frame is emitted to the host.
        let cancel_line = outbound_line(rx.recv().await.expect("cancel frame emitted"));
        let cancel: Value = serde_json::from_str(&cancel_line).unwrap();
        assert_eq!(cancel["type"], "control_cancel_request");
        assert!(cancel["request_id"].is_string());
    }

    #[tokio::test]
    async fn duplicate_control_response_for_resolved_tool_use_is_dropped() {
        // §1.5: resolve once (tracking the tool_use_id), then a duplicate
        // response (orphan, same toolUseID) is dropped without re-resolving.
        let (plane, _rx) = plane_with_channel();
        let (req_id, fut) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype": "can_use_tool"})),
                Some("tu-dup".to_string()),
            )
            .await;
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "toolUseID": "tu-dup"}),
            ))
            .await;
        assert_eq!(fut.await.unwrap().unwrap().value["behavior"], "allow");

        // The duplicate has a NEW request_id (orphan) but the same toolUseID.
        let dup = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "late-redelivery",
                "response": {"behavior": "allow", "toolUseID": "tu-dup"}
            }
        });
        // Must not panic / double-resolve; the tool_use_id is in the resolved ring.
        plane
            .resolve_response(&Utf16JsonProjection::plain(dup.clone()))
            .await;
        assert!(plane.is_resolved("tu-dup").await);
    }

    #[tokio::test]
    async fn fail_all_pending_rejects_in_flight_requests() {
        // §EOF: stdin close must reject every pending control_request so an
        // awaiting gate does not hang forever.
        let (plane, _rx) = plane_with_channel();
        let (_id1, fut1) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype": "can_use_tool"})),
                Some("a".into()),
            )
            .await;
        let (_id2, fut2) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype": "can_use_tool"})),
                Some("b".into()),
            )
            .await;
        plane
            .fail_all_pending("Tool permission stream closed before response received")
            .await;
        assert_eq!(
            fut1.await.unwrap().unwrap_err(),
            "Tool permission stream closed before response received"
        );
        assert_eq!(
            fut2.await.unwrap().unwrap_err(),
            "Tool permission stream closed before response received"
        );
        // Drained tool_use_ids are tracked so a late re-delivery is still deduped.
        assert!(plane.is_resolved("a").await);
        assert!(plane.is_resolved("b").await);
    }

    #[tokio::test]
    async fn gate_hang_is_broken_by_fail_all_pending() {
        // The end-to-end #3 scenario: a gate awaiting can_use_tool is unblocked
        // (denied) when fail_all_pending fires on EOF.
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });
        // Drain the emitted can_use_tool request, then simulate stdin EOF.
        let _ = rx.recv().await.unwrap();
        plane
            .fail_all_pending("Tool permission stream closed before response received")
            .await;
        match check.await.unwrap() {
            PermissionDecision::Deny { reason } => {
                assert!(reason.contains("Tool permission stream closed before response received"));
            }
            other => panic!("expected Deny on EOF, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn orphan_response_is_dropped_without_panic() {
        let (plane, _rx) = plane_with_channel();
        plane
            .resolve_response(&success_response("does-not-exist", json!({})))
            .await;
        // No pending entry to resolve, and no recovery sink wired — warn+drop.
    }

    #[tokio::test]
    async fn genuine_orphan_with_sink_forwards_orphaned_permission_command() {
        let (plane, _rx) = plane_with_channel();
        let (orphan_tx, mut orphan_rx) = mpsc::unbounded_channel::<OrphanedPermission>();
        plane.set_orphan_sender(orphan_tx).await;

        // A genuine orphan: no pending request for this request_id, and the
        // payload's toolUseID is not a known-resolved duplicate.
        plane
            .resolve_response(&success_response(
                "no-such-request",
                json!({
                    "behavior": "allow",
                    "updatedInput": { "command": "ls -la" },
                    "toolUseID": "toolu_orphan_1",
                }),
            ))
            .await;

        let cmd = orphan_rx.try_recv().expect("orphan command forwarded");
        assert_eq!(cmd.tool_use_id.as_str(), "toolu_orphan_1");
        assert_eq!(cmd.permission_decision.value["behavior"], "allow");
        assert_eq!(
            cmd.permission_decision.value["updatedInput"]["command"],
            "ls -la"
        );
    }

    #[tokio::test]
    async fn duplicate_resolved_orphan_is_not_forwarded() {
        let (plane, mut rx) = plane_with_channel();
        let (orphan_tx, mut orphan_rx) = mpsc::unbounded_channel::<OrphanedPermission>();
        plane.set_orphan_sender(orphan_tx).await;

        // Drive a NORMAL allow round-trip so the toolUseID is tracked as resolved.
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({"command": "ls"});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });
        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        let tuid = frame["request"]["tool_use_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {}}),
            ))
            .await;
        assert_eq!(check.await.unwrap(), PermissionDecision::Allow);

        // A LATE duplicate `control_response` for the same (now-resolved) toolUseID
        // with no pending request must be deduped (§1.5), NOT forwarded as an
        // orphan (which would double-execute the tool).
        plane
            .resolve_response(&success_response(
                "late-duplicate",
                json!({"behavior": "allow", "updatedInput": {}, "toolUseID": tuid}),
            ))
            .await;
        assert!(
            orphan_rx.try_recv().is_err(),
            "a resolved-duplicate must not be forwarded for recovery"
        );
    }

    #[tokio::test]
    async fn gate_allow_maps_to_allow() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({"command": "ls"});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["request"]["subtype"], "can_use_tool");
        assert_eq!(frame["request"]["tool_name"], "Bash");
        assert!(frame["request"]["tool_use_id"].is_string());
        let req_id = frame["request_id"].as_str().unwrap().to_string();

        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {}}),
            ))
            .await;
        assert_eq!(check.await.unwrap(), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn gate_deny_maps_to_deny_with_message() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Write", &input).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "deny", "message": "not allowed"}),
            ))
            .await;
        assert_eq!(
            check.await.unwrap(),
            PermissionDecision::Deny {
                reason: "not allowed".to_string()
            }
        );
    }

    // PERM-GATE-WIRE-01: h1e / display_name port.
    #[test]
    fn display_name_ports_h1e_byte_exact() {
        // Simple tool name: title-case the single word.
        assert_eq!(permission_display_name("Bash"), "Bash");
        assert_eq!(permission_display_name("bash"), "Bash");
        // snake_case tool: `_`→space, title-case each word.
        assert_eq!(permission_display_name("web_fetch"), "Web Fetch");
        // MCP-namespaced tool: strip the `mcp__server__` prefix, keep the bare
        // tool name, then space+title-case.
        assert_eq!(
            permission_display_name("mcp__github__create_issue"),
            "Create Issue"
        );
        // Digits are word chars but unchanged by upper-casing.
        assert_eq!(permission_display_name("get_2fa_code"), "Get 2fa Code");
        // Trailing `__` ⇒ empty last segment ⇒ JS `pop() || e` falls back to the
        // full name.
        assert_eq!(permission_display_name("Foo__"), "Foo  ");
    }

    #[tokio::test]
    async fn can_use_tool_request_carries_display_name() {
        // The outbound can_use_tool control_request must include `display_name`
        // (claude-code sends it unconditionally).
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let _check =
            tokio::spawn(async move { gate.check("mcp__github__create_issue", &input).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["request"]["subtype"], "can_use_tool");
        assert_eq!(frame["request"]["tool_name"], "mcp__github__create_issue");
        assert_eq!(frame["request"]["display_name"], "Create Issue");
    }

    #[tokio::test]
    async fn gate_deny_interrupt_cancels_active_turn() {
        let (plane, mut rx) = plane_with_channel();
        let token = CancellationToken::new();
        plane.set_active_turn(token.clone()).await;
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "deny", "message": "stop", "interrupt": true}),
            ))
            .await;
        assert_eq!(
            check.await.unwrap(),
            PermissionDecision::Deny {
                reason: "stop".to_string()
            }
        );
        assert!(token.is_cancelled(), "deny+interrupt must cancel the turn");
    }

    #[tokio::test]
    async fn idle_cancel_does_not_poison_the_next_turn() {
        let (plane, _rx) = plane_with_channel();

        assert!(!plane.cancel_active_turn().await);
        assert!(!plane.is_busy().await);

        let next = CancellationToken::new();
        plane.set_active_turn(next.clone()).await;
        assert!(plane.is_busy().await);
        assert!(!next.is_cancelled());

        plane.clear_active_turn().await;
        assert!(!plane.is_busy().await);
        assert!(!next.is_cancelled());
    }

    #[tokio::test]
    async fn gate_error_response_maps_to_deny() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        let resp = json!({
            "type": "control_response",
            "response": {"subtype": "error", "request_id": req_id, "error": "host failure"}
        });
        plane
            .resolve_response(&Utf16JsonProjection::plain(resp.clone()))
            .await;
        assert_eq!(
            check.await.unwrap(),
            PermissionDecision::Deny {
                reason: "Tool permission request failed: host failure".to_string()
            }
        );
    }

    #[tokio::test]
    async fn gate_check_with_worker_sets_agent_id() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let worker = PromptWorker {
            name: "researcher".to_string(),
            team: None,
            is_async: false,
        };
        let check =
            tokio::spawn(async move { gate.check_with_worker("Bash", &input, Some(worker)).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["request"]["agent_id"], "researcher");
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {}}),
            ))
            .await;
        assert_eq!(check.await.unwrap(), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn context_uses_real_tool_use_id_and_returns_updated_input() {
        // #10: the request carries the dispatcher-supplied tool_use_id (not a
        // minted uuid). #2a: a non-empty updatedInput flows back as the rewrite.
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({"command": "ls"});
        let ctx = PermissionCheckContext {
            tool_use_id: Some("toolu_real_42".to_string()),
            decision_reason: Some("needs review".to_string()),
            ..PermissionCheckContext::default()
        };
        let check =
            tokio::spawn(async move { gate.check_with_context("Bash", &input, &ctx).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            frame["request"]["tool_use_id"], "toolu_real_42",
            "real id used"
        );
        assert_eq!(frame["request"]["decision_reason"], "needs review");
        let req_id = frame["request_id"].as_str().unwrap().to_string();

        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {"command": "ls -la"}}),
            ))
            .await;
        match check.await.unwrap() {
            PermissionOutcome::Allow { updated_input, .. } => {
                assert_eq!(
                    updated_input,
                    Some(Utf16JsonProjection::plain(json!({"command": "ls -la"})))
                );
            }
            other => panic!("expected Allow with updated_input, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_emits_structured_optional_permission_metadata() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let ctx = PermissionCheckContext {
            tool_use_id: Some("toolu_9".to_string()),
            permission_suggestions: Some(json!([
                {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "ls *"}], "behavior": "allow", "destination": "session"}
            ])),
            blocked_path: Some("/etc/secret".to_string()),
            classifier_approvable: Some(false),
            requires_user_interaction: true,
            suppress_always_allow_rule: true,
            matched_ask_rule: Some(MatchedAskRule {
                source: "projectSettings".into(),
                tool_name: "Bash".into(),
                rule_content: Some("ls *".into()),
            }),
            ..PermissionCheckContext::default()
        };
        let check =
            tokio::spawn(async move { gate.check_with_context("Bash", &input, &ctx).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            frame["request"]["permission_suggestions"][0]["type"],
            "addRules"
        );
        assert_eq!(frame["request"]["blocked_path"], "/etc/secret");
        assert_eq!(frame["request"]["classifier_approvable"], false);
        assert_eq!(frame["request"]["requires_user_interaction"], true);
        assert_eq!(frame["request"]["suppress_always_allow_rule"], true);
        assert_eq!(
            frame["request"]["matched_ask_rule"],
            json!({
                "source": "projectSettings",
                "tool_name": "Bash",
                "rule_content": "ls *",
            })
        );
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {}}),
            ))
            .await;
        assert!(matches!(
            check.await.unwrap(),
            PermissionOutcome::Allow { .. }
        ));
    }

    #[tokio::test]
    async fn eligible_auto_response_is_forwarded_without_permission_updates() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({"command": "echo hi"});
        let ctx = PermissionCheckContext {
            auto_mode_prompt: Some(
                lingxi_core::host::permission_gate::AutoModePrompt::WorkflowBash,
            ),
            ..PermissionCheckContext::default()
        };
        let check =
            tokio::spawn(async move { gate.check_with_context("Bash", &input, &ctx).await });
        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["request"]["auto_mode_prompt"], "workflow_bash");
        let request_id = frame["request_id"].as_str().unwrap().to_string();
        plane
            .resolve_response(&success_response(
                &request_id,
                json!({"behavior": "allow_auto", "updatedInput": {}}),
            ))
            .await;
        assert!(matches!(
            check.await.unwrap(),
            PermissionOutcome::AllowAuto {
                updated_input: None
            }
        ));
    }

    #[tokio::test]
    async fn context_omits_optional_permission_metadata_when_absent() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });

        let line = outbound_line(rx.recv().await.unwrap());
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert!(
            frame["request"].get("permission_suggestions").is_none(),
            "absent suggestions ⇒ key omitted"
        );
        assert!(
            frame["request"].get("blocked_path").is_none(),
            "absent blocked_path ⇒ key omitted"
        );
        assert!(
            frame["request"].get("decision_reason").is_none(),
            "no decision reason ⇒ key omitted"
        );
        for key in [
            "classifier_approvable",
            "requires_user_interaction",
            "matched_ask_rule",
        ] {
            assert!(
                frame["request"].get(key).is_none(),
                "absent {key} ⇒ key omitted"
            );
        }
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {}}),
            ))
            .await;
        assert_eq!(check.await.unwrap(), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn allow_without_updated_input_is_denied() {
        // #7: the strict schema requires an `updatedInput` key on an allow; a
        // bare allow is malformed → deny.
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });
        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(&req_id, json!({"behavior": "allow"})))
            .await;
        match check.await.unwrap() {
            PermissionDecision::Deny { reason } => {
                assert!(reason.contains("malformed allow result"), "got {reason}");
            }
            other => panic!("expected Deny for malformed allow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_without_message_and_unknown_behavior_use_failed_family() {
        // #8/#16: the oracle's deny schema requires `message`, and any non-
        // allow/deny behavior is a schema-invalid result → both funnel through
        // the "Tool permission request failed: …" family (NOT "Permission denied"
        // / "returned an unknown behavior").
        for (payload, needle) in [
            (
                json!({"behavior": "deny"}),
                "Tool permission request failed: malformed deny result",
            ),
            (
                json!({"behavior": "banana"}),
                "Tool permission request failed: invalid permission result",
            ),
            (
                json!({"nonsense": true}),
                "Tool permission request failed: invalid permission result",
            ),
        ] {
            let (plane, mut rx) = plane_with_channel();
            let gate = StdioControlPermissionGate::new(plane.clone());
            let input = json!({});
            let check = tokio::spawn(async move { gate.check("Bash", &input).await });
            let line = outbound_line(rx.recv().await.unwrap());
            let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
                .as_str()
                .unwrap()
                .to_string();
            plane
                .resolve_response(&success_response(&req_id, payload))
                .await;
            match check.await.unwrap() {
                PermissionDecision::Deny { reason } => {
                    assert!(
                        reason.starts_with("Tool permission request failed: "),
                        "got {reason}"
                    );
                    assert!(reason.contains(needle), "got {reason}");
                }
                other => panic!("expected Deny, got {other:?}"),
            }
        }
    }

    // ── §2b: updatedPermissions parse + persist ──────────────────────────────

    #[test]
    fn parse_persistent_add_rules_models_each_rule() {
        // The host's addRules wire entry → one PermissionUpdate per rule, carrying
        // the parsed behavior + destination and the destination-derived source.
        let raw = json!({
            "type": "addRules",
            "rules": [
                { "toolName": "Bash", "ruleContent": "npm install" },
                { "toolName": "Read" }, // tool-wide (no ruleContent)
            ],
            "behavior": "allow",
            "destination": "localSettings",
        });
        let PersistentPermissionUpdate::AddRules(updates) =
            parse_persistent_permission_update(&raw).unwrap()
        else {
            panic!("expected addRules");
        };
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].rule.value.tool_name, "Bash");
        assert_eq!(
            updates[0].rule.value.rule_content.as_deref(),
            Some("npm install")
        );
        assert_eq!(updates[0].rule.behavior, PermissionBehavior::Allow);
        assert_eq!(
            updates[0].destination,
            PermissionUpdateDestination::LocalSettings
        );
        assert_eq!(
            updates[0].rule.source,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local)
        );
        // Tool-wide rule keeps ruleContent None.
        assert_eq!(updates[1].rule.value.tool_name, "Read");
        assert!(updates[1].rule.value.rule_content.is_none());
    }

    #[test]
    fn parse_persistent_rules_preserves_current_agent_tool_name() {
        let raw = json!({
            "type": "addRules",
            "rules": [{ "toolName": "Agent", "ruleContent": "general-purpose" }],
            "behavior": "deny",
            "destination": "userSettings",
        });
        let PersistentPermissionUpdate::AddRules(updates) =
            parse_persistent_permission_update(&raw).unwrap()
        else {
            panic!("expected addRules");
        };
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].rule.value.tool_name, "Agent");
        assert_eq!(updates[0].rule.behavior, PermissionBehavior::Deny);
        assert_eq!(
            updates[0].rule.source,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User)
        );
    }

    #[test]
    fn parse_persistent_update_models_all_variants_and_rejects_malformed_atomically() {
        assert!(matches!(
            parse_persistent_permission_update(&json!({
            "type": "replaceRules", "rules": [{ "toolName": "Bash" }],
            "behavior": "allow", "destination": "localSettings"
            })),
            Some(PersistentPermissionUpdate::ReplaceRules { .. })
        ));
        assert!(matches!(
            parse_persistent_permission_update(&json!({
            "type": "setMode", "mode": "auto", "destination": "localSettings"
            })),
            Some(PersistentPermissionUpdate::SetMode { .. })
        ));
        assert!(matches!(
            parse_persistent_permission_update(&json!({
                "type": "addDirectories", "directories": ["/extra"],
                "destination": "projectSettings"
            })),
            Some(PersistentPermissionUpdate::Directories { add: true, .. })
        ));

        // Unknown behavior / destination / missing rules are ignored, and one
        // malformed rule invalidates the complete union member.
        assert!(parse_persistent_permission_update(&json!({
            "type": "addRules", "rules": [{ "toolName": "Bash" }],
            "behavior": "bogus", "destination": "localSettings"
        }))
        .is_none());
        assert!(parse_persistent_permission_update(&json!({
            "type": "addRules", "rules": [{ "toolName": "Bash" }],
            "behavior": "allow", "destination": "bogus"
        }))
        .is_none());
        assert!(parse_persistent_permission_update(&json!({
            "type": "addRules", "behavior": "allow", "destination": "localSettings"
        }))
        .is_none());
        assert!(parse_persistent_permission_update(&json!({
            "type": "addRules",
            "rules": [{ "toolName": "Read" }, { "toolName": "Bash", "ruleContent": 1 }],
            "behavior": "allow", "destination": "localSettings"
        }))
        .is_none());
        assert!(parse_persistent_permission_update(&json!({
            "type": "addDirectories",
            "directories": ["/extra", "bad\u{0000}path"],
            "destination": "localSettings"
        }))
        .is_none());
        assert_eq!(
            directory_update_null_byte_reason("addDirectories", "bad\0path"),
            "addDirectories carries a directory containing a null byte: bad\0path"
        );
        assert!(parse_persistent_permission_update(&json!("nonsense")).is_none());
    }

    #[tokio::test]
    async fn allow_carries_updated_permissions_through_outcome() {
        // §2b: an allow with `updatedPermissions` surfaces the raw wire array on the
        // outcome; a missing array is simply empty (not a deny).
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move {
            gate.check_with_context("Bash", &input, &PermissionCheckContext::default())
                .await
        });
        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({
                    "behavior": "allow",
                    "updatedInput": {},
                    "updatedPermissions": [{
                        "type": "addRules",
                        "rules": [{ "toolName": "Bash" }],
                        "behavior": "allow",
                        "destination": "localSettings"
                    }],
                }),
            ))
            .await;
        match check.await.unwrap() {
            PermissionOutcome::Allow {
                permission_updates, ..
            } => {
                assert_eq!(permission_updates.len(), 1);
                assert_eq!(permission_updates[0]["type"], "addRules");
            }
            other => panic!("expected Allow with permission_updates, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_carries_host_decision_classification() {
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move {
            gate.check_with_context("Bash", &input, &PermissionCheckContext::default())
                .await
        });
        let line = outbound_line(rx.recv().await.unwrap());
        let request_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &request_id,
                json!({
                    "behavior": "allow",
                    "updatedInput": {},
                    "decisionClassification": "user_permanent"
                }),
            ))
            .await;

        assert!(matches!(
            check.await.unwrap(),
            PermissionOutcome::Allow {
                decision_classification: Some(
                    lingxi_core::host::permission_gate::ToolDecisionClassification::UserPermanent
                ),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn malformed_updated_permissions_is_not_a_deny() {
        // The oracle ignores a malformed `updatedPermissions` (`.catch(undefined)`)
        // — it must NOT turn the allow into a deny. A non-array value → empty.
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone());
        let input = json!({});
        let check = tokio::spawn(async move { gate.check("Bash", &input).await });
        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({"behavior": "allow", "updatedInput": {}, "updatedPermissions": "garbage"}),
            ))
            .await;
        assert_eq!(check.await.unwrap(), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn allow_with_persist_writes_rule_to_settings() {
        // End-to-end: a gate WITH persist paths writes the host's addRules rule to
        // the resolved settings file (claude-code `persistPermissionUpdates`).
        let tmp = std::env::temp_dir().join(format!("lx-p5-2b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let (plane, mut rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane.clone()).with_persist(paths);
        let input = json!({});
        let check = tokio::spawn(async move {
            gate.check_with_context("Bash", &input, &PermissionCheckContext::default())
                .await
        });
        let line = outbound_line(rx.recv().await.unwrap());
        let req_id = serde_json::from_str::<Value>(&line).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        plane
            .resolve_response(&success_response(
                &req_id,
                json!({
                    "behavior": "allow",
                    "updatedInput": {},
                    "updatedPermissions": [{
                        "type": "addRules",
                        "rules": [{ "toolName": "Bash", "ruleContent": "npm install" }],
                        "behavior": "allow",
                        "destination": "localSettings"
                    }],
                }),
            ))
            .await;
        let outcome = check.await.unwrap();
        assert!(matches!(outcome, PermissionOutcome::Allow { .. }));
        // The rule landed in <cwd>/.lingxi/settings.local.json.
        let path = tmp.join("proj/.lingxi/settings.local.json");
        let written = std::fs::read_to_string(&path).expect("settings.local.json written");
        let v: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash(npm install)"]));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn nul_add_directories_update_is_not_persisted_atomically() {
        let tmp =
            std::env::temp_dir().join(format!("lx-p5-nul-directory-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let (plane, _rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane).with_persist(paths);
        gate.persist_permission_updates_to_disk(&[json!({
            "type": "addDirectories",
            "directories": ["/extra", "bad\u{0000}path"],
            "destination": "localSettings"
        })])
        .await;

        assert!(
            !tmp.join("proj/.lingxi/settings.local.json").exists(),
            "a mixed valid/NUL directory update must not create a settings write"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn persist_updated_permissions_handles_every_file_backed_variant() {
        let tmp =
            std::env::temp_dir().join(format!("lx-p5-permission-union-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let (plane, _rx) = plane_with_channel();
        let gate = StdioControlPermissionGate::new(plane).with_persist(paths);
        gate.persist_permission_updates_to_disk(&[
            json!({
                "type": "replaceRules",
                "rules": [{"toolName": "Read"}, {"toolName": "Edit", "ruleContent": "src/**"}],
                "behavior": "allow",
                "destination": "localSettings"
            }),
            json!({
                "type": "removeRules",
                "rules": [{"toolName": "Read"}],
                "behavior": "allow",
                "destination": "localSettings"
            }),
            json!({
                "type": "addRules",
                "rules": [{"toolName": "Bash", "ruleContent": "cargo test"}],
                "behavior": "deny",
                "destination": "localSettings"
            }),
            json!({
                "type": "setMode",
                "mode": "auto",
                "destination": "localSettings"
            }),
            json!({
                "type": "addDirectories",
                "directories": ["/extra", "/removed"],
                "destination": "localSettings"
            }),
            json!({
                "type": "removeDirectories",
                "directories": ["/removed"],
                "destination": "localSettings"
            }),
        ])
        .await;

        let path = tmp.join("proj/.lingxi/settings.local.json");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["permissions"]["allow"], json!(["Edit(src/**)"]));
        assert_eq!(value["permissions"]["deny"], json!(["Bash(cargo test)"]));
        assert_eq!(value["permissions"]["defaultMode"], "auto");
        assert_eq!(
            value["permissions"]["additionalDirectories"],
            json!(["/extra"])
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod close_scope_tests {
    use super::*;
    #[tokio::test]
    async fn broker_retains_unpaired_utf16_in_requests_and_responses() {
        let (out, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(out));
        let request =
            Utf16JsonProjection::parse(r#"{"subtype":"can_use_tool","input":{"\ud800":"\udfff"}}"#)
                .unwrap();
        let (request_id, response) = plane.send_request(request, None).await;
        let Some(OutboundMsg::Line(line)) = frames.recv().await else {
            panic!("expected control request");
        };
        assert!(line.contains(r#""input":{"\ud800":"\udfff"}"#));
        let incoming = Utf16JsonProjection::parse(&format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{request_id}","response":{{"behavior":"allow","updatedInput":{{"\ud800":"\udfff"}}}}}}}}"#
        ))
        .unwrap();
        plane.resolve_response(&incoming).await;
        let payload = response.await.unwrap().unwrap();
        assert_eq!(
            payload.to_json_string().unwrap(),
            r#"{"behavior":"allow","updatedInput":{"\ud800":"\udfff"}}"#
        );
    }

    #[tokio::test]
    async fn orphan_permission_retains_the_exact_updated_input_projection() {
        let (out, _) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(out));
        let (orphans, mut commands) = tokio::sync::mpsc::unbounded_channel();
        plane.set_orphan_sender(orphans).await;
        let incoming = Utf16JsonProjection::parse(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"orphan","response":{"toolUseID":"toolu_late","behavior":"allow","updatedInput":{"value":"\ud800"}}}}"#,
        )
        .unwrap();
        plane.resolve_response(&incoming).await;
        let orphan = commands.recv().await.unwrap();
        assert_eq!(orphan.tool_use_id.as_str(), "toolu_late");
        assert!(orphan
            .permission_decision
            .to_json_string()
            .unwrap()
            .contains(r#""updatedInput":{"value":"\ud800"}"#));
    }

    #[tokio::test]
    async fn closure_rejects_existing_and_future_permission_waiters() {
        let (out, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(out));
        let (_, first) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype":"can_use_tool"})),
                None,
            )
            .await;
        assert!(frames.recv().await.is_some());
        plane.close_input("closed fixture").await;
        assert!(first.await.unwrap().is_err());
        let (_, later) = plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype":"can_use_tool"})),
                None,
            )
            .await;
        assert!(later.await.unwrap().is_err());
        assert!(
            frames.try_recv().is_err(),
            "no prompt may be queued after closure"
        );
    }
    #[tokio::test]
    async fn shutdown_cancels_an_owner_without_releasing_its_cwd_lease() {
        let (out, _) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(out));
        let lease = plane.lock_operation().await;
        let token = CancellationToken::new();
        plane.set_active_turn(token.clone()).await;
        plane.shutdown("fixture").await;
        assert!(token.is_cancelled());
        assert!(plane.is_busy().await);
        assert!(plane.try_lock_operation().is_none());
        plane.clear_active_turn().await;
        drop(lease);
        assert!(!plane.is_busy().await);
        assert!(plane.try_lock_operation().is_some());
    }
}
