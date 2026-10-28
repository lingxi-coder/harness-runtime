//! Session-owned Host execution for Native Mods Client UI controls.
//!
//! Module source is prepared from the loaded plugin and kept on the host. The
//! renderer can request a manifest or a drawing operation, but it cannot
//! supply executable source or choose Client props outside the latest
//! validated parent `ui_render` tree.

use super::client_source::PreparedClientModules;
use super::{
    ModDispatchScope, ModError, ModHost, ModSessionContext, ModUiInvalidationUpdate,
    ModUiRenderPace, RequestChannel,
};
use crate::mod_ui_fault::{
    dispatch_client_ui_fault, ClientUiComponent, ClientUiFaultHost, ClientUiFaultRegistry,
    ClientUiIdentity, ClientUiRenderGeneration, ClientUiRenderSite, UiClientFaultRequest,
    UiFaultHookEvent,
};
use crate::response::ExactHookText;
use lingxi_core::host::orchestrator::ModUiControlOutcome;
use lingxi_core::host::CancellationToken;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;

const MAX_CONTROL_UTF16: usize = 256;
const MAX_PARENT_CLIENT_UTF16: usize = 10_000;
const MAX_CLIENT_MESSAGE_UTF16: usize = 100_000;
const MAX_CLIENT_MESSAGE_VALUES: usize = 2_000;
const MAX_CLIENT_MESSAGE_DEPTH: usize = 32;
const MAX_PRESS_VALUE_UTF16: usize = 16_384;
const MAX_JS_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const CLIENT_POST_SETTLEMENT_BUDGET: Duration = Duration::from_secs(20);

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn host_plugin_name(plugin: &ExactHookText) -> Option<String> {
    // Plugin names in the host registry are Rust strings. Convert only when
    // the exact JavaScript identity is well-formed UTF-16; a lone surrogate
    // must not alias a replacement-character plugin through its display view.
    String::from_utf16(plugin.utf16_code_units.as_slice()).ok()
}

fn exact_text_projection(value: &ExactHookText) -> Utf16JsonProjection {
    value.json_projection()
}

fn host_generated_json(value: Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value)
        .expect("host-generated Mod UI JSON is representable")
}

fn client_pending_event(
    event: &Utf16JsonProjection,
    post_ack: &mut Option<ClientPostEventGuard>,
    worker_epoch: Option<u64>,
) -> Result<Utf16JsonProjection, ModError> {
    let mut pending = event.clone();
    if let Some(post_ack) = post_ack.as_mut() {
        let epoch = worker_epoch
            .or_else(|| client_post_u64(event, "/hostPostWorkerEpoch"))
            .ok_or_else(|| ModError::Protocol("queued Client post has no worker epoch".into()))?;
        pending
            .set_field(
                "hostPostWorkerEpoch",
                Utf16JsonProjection::parse(&epoch.to_string())
                    .expect("worker epoch is a valid JSON number"),
            )
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        post_ack.defer();
    }
    Ok(pending)
}

fn plain_text_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("a Rust string is valid UTF-16")
}

fn client_post_u64(value: &Utf16JsonProjection, pointer: &str) -> Option<u64> {
    let number = value.number_at(pointer)?;
    (number.is_finite()
        && number.fract() == 0.0
        && number >= 0.0
        && number <= MAX_JS_SAFE_INTEGER as f64)
        .then_some(number as u64)
}

fn client_post_ids(result: &Utf16JsonProjection) -> Result<Vec<u64>, ModError> {
    let count = result
        .array_len("/postIds")
        .ok_or_else(|| ModError::Protocol("Client runHeld result has no post ID list".into()))?;
    (0..count)
        .map(|index| {
            client_post_u64(result, &format!("/postIds/{index}")).ok_or_else(|| {
                ModError::Protocol("Client runHeld result has an invalid post ID".into())
            })
        })
        .collect()
}

enum ClientRunHeldResult {
    Stale,
    Rejected,
    Accepted(Vec<u64>),
}

fn client_run_held_result(result: &Utf16JsonProjection) -> Result<ClientRunHeldResult, ModError> {
    let fields = result
        .object_entries()
        .ok_or_else(|| ModError::Protocol("Client runHeld result must be an object".into()))?;
    if let Some(stale) = result.at_pointer("/stale") {
        // The worker only reports stale before dispatching the callback. Do
        // not turn contradictory or malformed replies into a safe no-op.
        if stale.as_bool() != Some(true) || fields.len() != 1 {
            return Err(ModError::Protocol(
                "Client runHeld result has an invalid stale status".into(),
            ));
        }
        return Ok(ClientRunHeldResult::Stale);
    }
    let accepted = result
        .at_pointer("/accepted")
        .and_then(|value| value.as_bool())
        .ok_or_else(|| ModError::Protocol("Client runHeld result has no accepted flag".into()))?;
    let post_ids = client_post_ids(result)?;
    // The worker emits exactly accepted and postIds after dispatch. Unknown
    // fields could contradict acceptance or describe a fault, so never treat
    // them as either a safe rejection or a successful execution.
    if fields.len() != 2 {
        return Err(ModError::Protocol(
            "Client runHeld result has unexpected fields".into(),
        ));
    }
    if accepted {
        Ok(ClientRunHeldResult::Accepted(post_ids))
    } else if post_ids.is_empty() {
        Ok(ClientRunHeldResult::Rejected)
    } else {
        Err(ModError::Protocol(
            "Client runHeld rejected with post IDs".into(),
        ))
    }
}

/// Convert worker-private numeric held handles into the public Client frame's
/// plugin-scoped `press` carrier, without materializing the worker tree as a
/// serde value or losing exact string/key UTF-16 units.
fn public_client_tree_projection(
    tree: &Utf16JsonProjection,
    plugin: &str,
) -> Option<Utf16JsonProjection> {
    fn lower_children(value: &Utf16JsonProjection, plugin: &str) -> Option<Utf16JsonProjection> {
        if let Some(length) = value.array_len("") {
            let mut children = Vec::with_capacity(length);
            for index in 0..length {
                let child = value.subprojection(&format!("/{index}")).ok()?;
                let lowered = if child.array_len("").is_some()
                    || child.object_entries().is_some() && child.string_at("/type").is_some()
                {
                    lower_children(&child, plugin)?
                } else {
                    child
                };
                children.push(lowered);
            }
            return Utf16JsonProjection::array(children).ok();
        }

        if value.object_entries().is_none() || value.string_at("/type").is_none() {
            return Some(value.clone());
        }

        let node_type = value.string_at("/type").unwrap_or_default();
        let requires_private_press = matches!(node_type, "Button" | "Input" | "Select");
        let held_key = "held".encode_utf16().collect::<Vec<_>>();
        let children_key = "children".encode_utf16().collect::<Vec<_>>();
        let press_key = "press".encode_utf16().collect::<Vec<_>>();
        if node_type == "Markdown"
            && (value.subprojection_property_units(&held_key).is_ok()
                || value.subprojection_property_units(&press_key).is_ok())
        {
            return None;
        }
        let held = requires_private_press
            .then(|| value.subprojection_property_units(&held_key).ok())
            .flatten();
        let handle = if let Some(held) = &held {
            if value.subprojection_property_units(&press_key).is_ok() {
                return None;
            }
            let number = held.number_at("")?;
            if !number.is_finite()
                || number.fract() != 0.0
                || number <= 0.0
                || number > MAX_JS_SAFE_INTEGER as f64
            {
                return None;
            }
            Some(held.clone())
        } else if requires_private_press {
            return None;
        } else {
            None
        };

        let entries = value.object_entries()?;
        let mut press = handle
            .map(|handle| {
                Utf16JsonProjection::object(vec![
                    ("plugin".into(), plain_text_projection(plugin)),
                    ("handle".into(), handle),
                ])
            })
            .transpose()
            .ok()?;
        let mut fields = Vec::with_capacity(entries.len());
        for entry in entries {
            if requires_private_press && entry.code_units == held_key {
                if let Some(press) = press.take() {
                    fields.push((press_key.clone(), press));
                }
                continue;
            }
            let child = value.subprojection_property_units(entry.code_units).ok()?;
            let child = if entry.code_units == children_key {
                lower_children(&child, plugin)?
            } else {
                child
            };
            fields.push((entry.code_units.to_vec(), child));
        }
        if press.is_some() {
            return None;
        }
        Utf16JsonProjection::object_units(fields).ok()
    }

    lower_children(tree, plugin)
}

#[derive(Default)]
pub(super) struct ClientUiHostState {
    data: StdMutex<ClientUiHostData>,
    post_actions: Arc<ClientPostAckRegistry>,
    fault_registry: ClientUiFaultRegistry,
    next_revision: AtomicU64,
    next_client_environment_epoch: AtomicU64,
    next_runtime_id: AtomicU64,
    next_invalidation_id: AtomicU64,
    next_post_action_id: AtomicU64,
    #[cfg(test)]
    held_result_received: AtomicU64,
    #[cfg(test)]
    finalize_gate: StdMutex<Option<ClientHeldFinalizeGate>>,
}

#[cfg(test)]
struct ClientHeldFinalizeGate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct ClientPostAckRegistry {
    actions: StdMutex<HashMap<String, ClientPostActionState>>,
}

struct ClientPostActionState {
    runtime_id: String,
    worker_epoch: Option<u64>,
    sender: mpsc::UnboundedSender<ClientPostAck>,
    cancellation: CancellationToken,
}

enum ClientPostAck {
    Settled { worker_epoch: u64, post_id: u64 },
    Failed { worker_epoch: u64, reason: String },
}

pub(super) struct ClientPostActionGuard {
    registry: Arc<ClientPostAckRegistry>,
    action_id: String,
    runtime_id: String,
    worker_epoch: Option<u64>,
    receiver: mpsc::UnboundedReceiver<ClientPostAck>,
    cancellation: CancellationToken,
    settled: bool,
}

impl ClientPostActionGuard {
    fn action_id(&self) -> &str {
        &self.action_id
    }

    pub(super) fn bind_worker_epoch(&mut self, worker_epoch: u64) -> Result<(), ModError> {
        let mut actions = lock(&self.registry.actions);
        let Some(action) = actions.get_mut(&self.action_id) else {
            return Err(ModError::Unavailable(
                "Client post action was cancelled before worker dispatch".into(),
            ));
        };
        if action.runtime_id != self.runtime_id
            || action
                .worker_epoch
                .is_some_and(|current| current != worker_epoch)
        {
            return Err(ModError::Protocol(
                "Client post action changed runtime or worker epoch".into(),
            ));
        }
        if action.cancellation.is_cancelled() {
            return Err(ModError::Unavailable(
                "Client post action was cancelled before worker dispatch".into(),
            ));
        }
        action.worker_epoch = Some(worker_epoch);
        self.worker_epoch = Some(worker_epoch);
        Ok(())
    }

    async fn wait_for_posts(&mut self, post_ids: &[u64]) -> Result<(), ModError> {
        let Some(worker_epoch) = self.worker_epoch else {
            return Err(ModError::Protocol(
                "Client post action has no worker epoch".into(),
            ));
        };
        let mut pending = post_ids.iter().copied().collect::<HashSet<_>>();
        if pending.len() != post_ids.len() {
            return Err(ModError::Protocol(
                "Client post action contains duplicate post IDs".into(),
            ));
        }
        if pending.is_empty() {
            if self.cancellation.is_cancelled() {
                return Err(ModError::Unavailable(
                    "Client post action was cancelled".into(),
                ));
            }
            self.settled = true;
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + CLIENT_POST_SETTLEMENT_BUDGET;
        while !pending.is_empty() {
            let ack = tokio::time::timeout_at(deadline, async {
                tokio::select! {
                    biased;
                    ack = self.receiver.recv() => ack,
                    _ = self.cancellation.cancelled() => None,
                }
            })
            .await
            .map_err(|_| {
                ModError::Unavailable("Client post handling did not settle in time".into())
            })?
            .ok_or_else(|| ModError::Unavailable("Client post handling was cancelled".into()))?;
            match ack {
                ClientPostAck::Settled {
                    worker_epoch: ack_epoch,
                    post_id,
                } if ack_epoch == worker_epoch => {
                    pending.remove(&post_id);
                }
                ClientPostAck::Failed {
                    worker_epoch: ack_epoch,
                    reason,
                } if ack_epoch == worker_epoch => {
                    return Err(ModError::Unavailable(format!(
                        "Client post handling failed: {reason}"
                    )));
                }
                ClientPostAck::Settled { .. } | ClientPostAck::Failed { .. } => {
                    // An old worker epoch cannot settle a current action.
                }
            }
        }
        if self.cancellation.is_cancelled() {
            return Err(ModError::Unavailable(
                "Client post action was cancelled".into(),
            ));
        }
        self.settled = true;
        Ok(())
    }

    fn fail(&self, reason: impl Into<String>) {
        let Some(worker_epoch) = self.worker_epoch else {
            return;
        };
        self.registry.notify(
            &self.action_id,
            &self.runtime_id,
            worker_epoch,
            ClientPostAck::Failed {
                worker_epoch,
                reason: reason.into(),
            },
        );
    }
}

impl Drop for ClientPostActionGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.fail("Client post action was cancelled");
        }
        self.registry.remove(&self.action_id, &self.runtime_id);
    }
}

struct ClientPostEventGuard {
    registry: Arc<ClientPostAckRegistry>,
    action_id: Option<String>,
    runtime_id: String,
    worker_epoch: Option<u64>,
    post_id: Option<u64>,
    cancellation: CancellationToken,
    deferred: bool,
    settled: bool,
}

impl ClientPostEventGuard {
    fn new(
        registry: Arc<ClientPostAckRegistry>,
        event: &Utf16JsonProjection,
        worker_epoch: Option<u64>,
    ) -> Option<Self> {
        if event.string_at("/action") != Some("post") {
            return None;
        }
        let action_id = event.string_at("/postActionId").map(str::to_owned);
        action_id.as_ref()?;
        let runtime_id = event.string_at("/runtimeId").unwrap_or_default().to_owned();
        let worker_epoch = worker_epoch.or_else(|| client_post_u64(event, "/hostPostWorkerEpoch"));
        let cancellation = registry.cancellation_for(
            action_id.as_deref().expect("action id checked above"),
            &runtime_id,
            worker_epoch,
        );
        Some(Self {
            registry,
            action_id,
            runtime_id,
            worker_epoch,
            post_id: client_post_u64(event, "/postId"),
            cancellation,
            deferred: false,
            settled: false,
        })
    }

    fn defer(&mut self) {
        self.deferred = true;
    }

    fn complete(mut self) {
        if let (Some(worker_epoch), Some(post_id)) = (self.worker_epoch, self.post_id) {
            self.send(ClientPostAck::Settled {
                worker_epoch,
                post_id,
            });
        } else if let Some(worker_epoch) = self.worker_epoch {
            self.send(ClientPostAck::Failed {
                worker_epoch,
                reason: "Client post event has invalid correlation metadata".into(),
            });
        }
        self.settled = true;
    }

    fn fail(mut self, reason: &str) {
        if let Some(worker_epoch) = self.worker_epoch {
            self.send(ClientPostAck::Failed {
                worker_epoch,
                reason: reason.to_owned(),
            });
        }
        self.settled = true;
    }

    fn send(&self, ack: ClientPostAck) {
        if let (Some(action_id), Some(worker_epoch)) =
            (self.action_id.as_deref(), self.worker_epoch)
        {
            self.registry
                .notify(action_id, &self.runtime_id, worker_epoch, ack);
        }
    }
}

impl Drop for ClientPostEventGuard {
    fn drop(&mut self) {
        if !self.deferred && !self.settled {
            self.send(ClientPostAck::Failed {
                worker_epoch: self.worker_epoch.unwrap_or_default(),
                reason: "Client post event was discarded before handling completed".into(),
            });
        }
    }
}

impl ClientPostAckRegistry {
    fn register(
        self: &Arc<Self>,
        action_id: String,
        runtime_id: String,
    ) -> Result<ClientPostActionGuard, ModError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let mut actions = lock(&self.actions);
        if actions.contains_key(&action_id) {
            return Err(ModError::Protocol(
                "Client post action ID was reused".into(),
            ));
        }
        actions.insert(
            action_id.clone(),
            ClientPostActionState {
                runtime_id: runtime_id.clone(),
                worker_epoch: None,
                sender,
                cancellation: cancellation.clone(),
            },
        );
        Ok(ClientPostActionGuard {
            registry: Arc::clone(self),
            action_id,
            runtime_id,
            worker_epoch: None,
            receiver,
            cancellation,
            settled: false,
        })
    }

    fn notify(&self, action_id: &str, runtime_id: &str, worker_epoch: u64, ack: ClientPostAck) {
        let actions = lock(&self.actions);
        let Some(action) = actions.get(action_id) else {
            return;
        };
        if action.runtime_id != runtime_id
            || action
                .worker_epoch
                .is_some_and(|current| current != worker_epoch)
        {
            return;
        }
        let _ = action.sender.send(ack);
    }

    fn cancellation_for(
        &self,
        action_id: &str,
        runtime_id: &str,
        worker_epoch: Option<u64>,
    ) -> CancellationToken {
        let actions = lock(&self.actions);
        let token = actions
            .get(action_id)
            .filter(|action| {
                action.runtime_id == runtime_id
                    && worker_epoch.is_some()
                    && action.worker_epoch == worker_epoch
            })
            .map(|action| action.cancellation.clone());
        token.unwrap_or_else(|| {
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            cancelled
        })
    }

    fn remove(&self, action_id: &str, runtime_id: &str) {
        let mut actions = lock(&self.actions);
        if actions
            .get(action_id)
            .is_some_and(|action| action.runtime_id == runtime_id)
        {
            if let Some(action) = actions.remove(action_id) {
                action.cancellation.cancel();
            }
        }
    }

    fn fail_epoch(&self, worker_epoch: u64, reason: &str) {
        let actions = lock(&self.actions);
        for action in actions.values() {
            if action.worker_epoch == Some(worker_epoch) {
                action.cancellation.cancel();
                let _ = action.sender.send(ClientPostAck::Failed {
                    worker_epoch,
                    reason: reason.to_owned(),
                });
            }
        }
    }

    fn fail_runtime(&self, runtime_id: &str, reason: &str) {
        let actions = lock(&self.actions);
        for action in actions.values() {
            if action.runtime_id == runtime_id {
                action.cancellation.cancel();
                if let Some(worker_epoch) = action.worker_epoch {
                    let _ = action.sender.send(ClientPostAck::Failed {
                        worker_epoch,
                        reason: reason.to_owned(),
                    });
                }
            }
        }
    }

    fn fail_all(&self, reason: &str) {
        let actions = lock(&self.actions);
        for action in actions.values() {
            action.cancellation.cancel();
            if let Some(worker_epoch) = action.worker_epoch {
                let _ = action.sender.send(ClientPostAck::Failed {
                    worker_epoch,
                    reason: reason.to_owned(),
                });
            }
        }
    }
}

#[derive(Default)]
struct ClientUiHostData {
    modules: HashMap<String, PreparedClientModules>,
    client_runtime_epochs: HashMap<String, u64>,
    plugin_by_storage: HashMap<String, String>,
    parents: HashMap<ClientUiRenderSite, ParentRender>,
    latest_started: HashMap<ClientUiRenderSite, u64>,
    parent_control_latest: HashMap<ParentControlSite, u64>,
    parent_control_renders: HashMap<ParentControlSite, ParentControlRender>,
    pending_tokens: HashMap<(ClientUiRenderSite, u64), ClientUiRenderGeneration>,
    instances: HashMap<String, ClientUiInstance>,
    instance_by_identity: HashMap<ClientUiInstanceKey, String>,
}

struct ParentRender {
    revision: u64,
    token: Option<ClientUiRenderGeneration>,
    committed_generation: Option<u64>,
    clients: HashMap<ClientUiIdentity, Utf16JsonProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ParentControlSite {
    surface: String,
    component: ClientUiComponent,
    request_id: Vec<u16>,
}

struct ParentControlRender {
    revision: u64,
    press_actions: Vec<ParentUiPressAction>,
}

#[derive(Clone)]
struct ParentUiPressAction {
    site: ParentControlSite,
    surface: String,
    plugin: Vec<u16>,
    handle: u64,
    element: Option<Vec<u16>>,
    kind: String,
    pressable_links: Option<Vec<Vec<u16>>>,
    select_values: HashSet<Vec<u16>>,
}

enum ClientWorkerOperationError {
    Fault {
        phase: String,
        reason: ExactHookText,
    },
    // The held callback was invoked (or the worker died before it could
    // answer); a side effect may already have occurred and the VM is failed.
    UnknownFault {
        phase: String,
        reason: ExactHookText,
    },
    Host(ModError),
}

impl From<ModError> for ClientWorkerOperationError {
    fn from(error: ModError) -> Self {
        Self::Host(error)
    }
}

impl From<ClientWorkerOperationError> for ModError {
    fn from(error: ClientWorkerOperationError) -> Self {
        match error {
            ClientWorkerOperationError::Fault { phase, reason }
            | ClientWorkerOperationError::UnknownFault { phase, reason } => {
                ModError::Hook(format!("Client surface {phase} failed: {}", reason.display))
            }
            ClientWorkerOperationError::Host(error) => error,
        }
    }
}

fn worker_fault_response(
    runtime_id: Option<&str>,
    revision: u64,
    plugin: &ExactHookText,
    module: &ExactHookText,
    phase: &str,
    reason: &ExactHookText,
) -> Utf16JsonProjection {
    let normalized_reason =
        crate::mod_ui_fault::normalize_client_fault_reason(plugin, module, reason);
    let mut response = Utf16JsonProjection::object(vec![
        ("handled".into(), host_generated_json(json!(false))),
        (
            "renderRevision".into(),
            Utf16JsonProjection::parse(&revision.to_string())
                .expect("revision is a valid JSON number"),
        ),
    ])
    .expect("worker fault response fields form an object");
    if let Some(runtime_id) = runtime_id {
        response
            .set_field("runtimeId", plain_text_projection(runtime_id))
            .expect("runtime id is a valid response field");
    }
    let fault = Utf16JsonProjection::object(vec![
        ("phase".into(), plain_text_projection(phase)),
        ("reason".into(), exact_text_projection(&normalized_reason)),
        ("source".into(), plain_text_projection("worker")),
    ])
    .expect("worker fault details form an object");
    response
        .set_field("fault", fault)
        .expect("worker fault response accepts fault details");
    response
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ClientUiInstanceKey {
    site: ClientUiRenderSite,
    identity: ClientUiIdentity,
}

#[derive(Clone)]
struct ClientUiInstance {
    key: ClientUiInstanceKey,
    runtime_id: String,
    revision: u64,
    worker_generation: Option<u64>,
    // Worker-minted identity for the shared VM, distinct from manifest hash.
    worker_environment_id: Option<String>,
    pending_events: Vec<Utf16JsonProjection>,
    draining_pending_events: bool,
    props: Utf16JsonProjection,
    mounted: bool,
    worker_fault: Option<ClientUiWorkerFault>,
    worker_fault_snapshot: ClientFaultSnapshotState,
    worker_fault_event_dispatched: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClientFaultSnapshotState {
    Pending,
    Publishing,
    Sent,
}

/// Reserve the one-shot fault publication without acknowledging it early.
/// A dropped/cancelled future releases the reservation for later replay.
struct ClientFaultSnapshotClaim<'a> {
    state: &'a ClientUiHostState,
    runtime_id: String,
    committed: bool,
}

impl ClientFaultSnapshotClaim<'_> {
    fn commit(mut self) {
        if let Some(instance) = lock(&self.state.data).instances.get_mut(&self.runtime_id) {
            if instance.worker_fault_snapshot == ClientFaultSnapshotState::Publishing {
                instance.worker_fault_snapshot = ClientFaultSnapshotState::Sent;
            }
        }
        self.committed = true;
    }
}

impl Drop for ClientFaultSnapshotClaim<'_> {
    fn drop(&mut self) {
        if self.committed { return; }
        if let Some(instance) = lock(&self.state.data).instances.get_mut(&self.runtime_id) {
            if instance.worker_fault_snapshot == ClientFaultSnapshotState::Publishing {
                instance.worker_fault_snapshot = ClientFaultSnapshotState::Pending;
            }
        }
    }
}

#[derive(Clone)]
struct ClientUiWorkerFault {
    phase: String,
    reason: ExactHookText,
}

/// Own the Worker action until Host post settlement and follow-up render have
/// completed. Drop sends cancellation through RequestChannel and fences all
/// surfaces that shared the possibly contaminated VM.
struct ClientHeldLease<'a> {
    host: &'a ModHost,
    owner: ClientUiInstance,
    request: RequestChannel,
    committed: bool,
}

impl ClientHeldLease<'_> {
    async fn finalize(&mut self, action_id: &str) -> Result<(), ModError> {
        self.host.send_generated(&json!({
            "id":self.request.id, "kind":"ui.client.finalize", "postActionId":action_id,
        })).await?;
        let reply = self.host.receive(&mut self.request).await?;
        #[cfg(test)]
        {
            let gate = { lock(&self.host.client_ui.finalize_gate).take() };
            if let Some(gate) = gate {
                // Model cancellation after the Worker sends its ACK but
                // before the Host future can consume that acknowledgement.
                self.request.completed = false;
                gate.entered.notify_one();
                gate.release.notified().await;
                self.request.completed = true;
            }
        }
        if reply.string_at("/kind") != Some("ui.client.finalized") {
            self.request.completed = false;
            return Err(ModError::Unavailable("Client held action finalization failed with outcome unknown".into()));
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for ClientHeldLease<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.host.record_client_environment_fault(
                &self.owner, "run", &ExactHookText::from_text("Client held action ended with outcome unknown"),
            );
        }
    }
}

fn remove_instance_if_current(
    data: &mut ClientUiHostData,
    runtime_id: &str,
    key: &ClientUiInstanceKey,
) {
    data.instances.remove(runtime_id);
    if data.instance_by_identity.get(key).map(String::as_str) == Some(runtime_id) {
        data.instance_by_identity.remove(key);
    }
}

impl ClientUiHostState {
    fn begin_post_action(&self, runtime_id: &str) -> Result<ClientPostActionGuard, ModError> {
        let sequence = self.next_post_action_id.fetch_add(1, Ordering::Relaxed) + 1;
        if sequence == 0 || sequence > MAX_JS_SAFE_INTEGER {
            return Err(ModError::Unavailable(
                "Client post action ID space is exhausted".into(),
            ));
        }
        self.post_actions
            .register(format!("client-post-{sequence}"), runtime_id.to_owned())
    }

    pub(super) fn fail_post_actions_for_epoch(&self, worker_epoch: u64, reason: &str) {
        self.post_actions.fail_epoch(worker_epoch, reason);
    }

    pub(super) fn fail_all_post_actions(&self, reason: &str) {
        self.post_actions.fail_all(reason);
    }

    pub(super) fn fail_post_event(
        &self,
        event: &Utf16JsonProjection,
        worker_epoch: u64,
        reason: &str,
    ) {
        if let Some(post_ack) =
            ClientPostEventGuard::new(Arc::clone(&self.post_actions), event, Some(worker_epoch))
        {
            post_ack.fail(reason);
        }
    }

    fn fail_post_actions_for_runtime(&self, runtime_id: &str, reason: &str) {
        self.post_actions.fail_runtime(runtime_id, reason);
    }

    fn remove_instance_if_current(
        &self,
        data: &mut ClientUiHostData,
        runtime_id: &str,
        key: &ClientUiInstanceKey,
    ) {
        remove_instance_if_current(data, runtime_id, key);
        self.fail_post_actions_for_runtime(runtime_id, "Client runtime was removed");
    }

    pub(super) fn install_loaded_plugin(
        &self,
        plugin: &str,
        storage_id: &str,
        prepared: Option<PreparedClientModules>,
    ) {
        let mut data = lock(&self.data);
        data.plugin_by_storage
            .insert(storage_id.to_owned(), plugin.to_owned());
        if let Some(prepared) = prepared {
            data.modules.insert(plugin.to_owned(), prepared);
            let epoch = self
                .next_client_environment_epoch
                .fetch_add(1, Ordering::AcqRel)
                + 1;
            assert!(
                epoch > 0 && epoch <= MAX_JS_SAFE_INTEGER,
                "Client environment epoch space exhausted"
            );
            data.client_runtime_epochs.insert(plugin.to_owned(), epoch);
        } else {
            data.modules.remove(plugin);
            data.client_runtime_epochs.remove(plugin);
        }
    }

    pub(super) fn plugin_for_storage(&self, storage_id: &str) -> Option<String> {
        lock(&self.data).plugin_by_storage.get(storage_id).cloned()
    }

    pub(super) fn unload_storage(&self, storage_id: &str) -> Vec<ClientUiRenderSite> {
        let (removed_plugin, affected_sites, removed_runtimes) = {
            let mut data = lock(&self.data);
            if let Some(plugin) = data.plugin_by_storage.remove(storage_id) {
                let still_loaded = data
                    .plugin_by_storage
                    .values()
                    .any(|loaded| loaded == &plugin);
                if !still_loaded {
                    data.modules.remove(&plugin);
                    data.client_runtime_epochs.remove(&plugin);
                    let removed_runtimes = data
                        .instances
                        .iter()
                        .filter(|(_, instance)| {
                            host_plugin_name(instance.key.identity.plugin()).as_deref()
                                == Some(plugin.as_str())
                        })
                        .map(|(runtime_id, _)| runtime_id.clone())
                        .collect::<Vec<_>>();
                    data.instances.retain(|_, instance| {
                        host_plugin_name(instance.key.identity.plugin()).as_deref()
                            != Some(plugin.as_str())
                    });
                    data.instance_by_identity.retain(|key, _| {
                        host_plugin_name(key.identity.plugin()).as_deref() != Some(plugin.as_str())
                    });
                    let mut affected_sites = HashSet::new();
                    for (site, parent) in data.parents.iter_mut() {
                        if parent.clients.keys().any(|identity| {
                            host_plugin_name(identity.plugin()).as_deref() == Some(plugin.as_str())
                        }) {
                            affected_sites.insert(site.clone());
                        }
                        parent.clients.retain(|identity, _| {
                            host_plugin_name(identity.plugin()).as_deref() != Some(plugin.as_str())
                        });
                    }
                    for (site, parent) in data.parent_control_renders.iter_mut() {
                        if site.surface == "desktop"
                            && parent.press_actions.iter().any(|action| {
                                host_plugin_name(&ExactHookText::from_utf16(action.plugin.clone()))
                                    .as_deref()
                                    == Some(plugin.as_str())
                            })
                        {
                            if let Ok(site) = ClientUiRenderSite::new(
                                site.component,
                                ExactHookText::from_utf16(site.request_id.clone()),
                            ) {
                                affected_sites.insert(site);
                            }
                        }
                        parent.press_actions.retain(|action| {
                            host_plugin_name(&ExactHookText::from_utf16(action.plugin.clone()))
                                .as_deref()
                                != Some(plugin.as_str())
                        });
                    }
                    (
                        Some(plugin),
                        affected_sites.into_iter().collect(),
                        removed_runtimes,
                    )
                } else {
                    // Unloading any storage disposes the worker's plugin
                    // environment. If another storage still owns the plugin,
                    // the prepared modules remain available for the next
                    // mount but that runtime needs a fresh local epoch.
                    let epoch = self
                        .next_client_environment_epoch
                        .fetch_add(1, Ordering::AcqRel)
                        + 1;
                    assert!(
                        epoch > 0 && epoch <= MAX_JS_SAFE_INTEGER,
                        "Client environment epoch space exhausted"
                    );
                    data.client_runtime_epochs.insert(plugin.clone(), epoch);
                    (None, Vec::new(), Vec::new())
                }
            } else {
                (None, Vec::new(), Vec::new())
            }
        };
        if let Some(plugin) = removed_plugin {
            self.fault_registry
                .remove_plugin_clients(&ExactHookText::from_text(plugin));
        }
        for runtime_id in removed_runtimes {
            self.fail_post_actions_for_runtime(&runtime_id, "Client plugin was unloaded");
        }
        affected_sites
    }

    fn next_revision(&self) -> Result<u64, ModError> {
        let revision = self.next_revision.fetch_add(1, Ordering::AcqRel) + 1;
        if revision == 0 || revision > MAX_JS_SAFE_INTEGER {
            return Err(ModError::Unavailable(
                "Mod UI render revision space exhausted".into(),
            ));
        }
        Ok(revision)
    }

    fn next_runtime_id(&self, host_id: u64) -> String {
        let counter = self.next_runtime_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("mod-client-{host_id}-{counter}")
    }
}

/// Validate a canonical Native Mods Client UI request without requiring a
/// live session. Core hosts use this before projecting a no-Mod fallback.
pub(super) fn normalize_control_request(
    request: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ModError> {
    request
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let subtype = request
        .string_at("/subtype")
        .ok_or_else(|| ModError::Hook("Mod UI control request needs a subtype".into()))?;
    let normalized = match subtype {
        "ui_render" => {
            let mut result = pick_projection_fields(
                request,
                "",
                &[
                    "subtype",
                    "surface",
                    "component",
                    "instance_id",
                    "props",
                    "client_id",
                    "content_rows",
                ],
            )?;
            if request.get_property("viewport").is_some() {
                let viewport = pick_projection_fields(
                    request,
                    "/viewport",
                    &["columns", "rows", "isFullscreen"],
                )?;
                result
                    .set_field("viewport", viewport)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            if request.get_property("on_screen").is_some() {
                let on_screen = if request
                    .at_pointer("/on_screen")
                    .is_some_and(|value| value.is_null())
                {
                    request
                        .subprojection("/on_screen")
                        .map_err(|error| ModError::Protocol(error.to_string()))?
                } else {
                    pick_projection_fields(request, "/on_screen", &["first", "last", "of"])?
                };
                result
                    .set_field("on_screen", on_screen)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            if let Some(length) = request.array_len("/keyed") {
                let rows = (0..length)
                    .map(|index| {
                        pick_projection_fields(
                            request,
                            &format!("/keyed/{index}"),
                            &["plugin", "key", "top", "bottom"],
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let keyed = Utf16JsonProjection::array(rows)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                result
                    .set_field("keyed", keyed)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            } else if request.get_property("keyed").is_some() {
                let keyed = request
                    .subprojection("/keyed")
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                result
                    .set_field("keyed", keyed)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            if request.get_property("bench").is_some() {
                let bench = pick_projection_fields(request, "/bench", &["seq", "t0"])?;
                result
                    .set_field("bench", bench)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            result
        }
        "ui_client_module" => pick_projection_fields(request, "", &["subtype", "plugin"])?,
        "ui_message" => {
            let mut result = pick_projection_fields(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                ],
            )?;
            if request.get_property("data").is_some() {
                let data = request
                    .subprojection("/data")
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                result
                    .set_field("data", data)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            result
        }
        "ui_client_fault" => pick_projection_fields(
            request,
            "",
            &[
                "subtype",
                "plugin",
                "component",
                "instance_id",
                "client",
                "module",
                "phase",
                "reason",
            ],
        )?,
        "ui_client_press" => {
            let mut result = pick_projection_fields(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "element",
                ],
            )?;
            let event_type = request
                .string_at("/event/type")
                .ok_or_else(|| ModError::Hook("ui_client_press event needs a type".into()))?;
            let fields = match event_type {
                "press" => &["type"][..],
                "input" => &["type", "kind", "value"][..],
                "select" => &["type", "value"][..],
                _ => {
                    return Err(ModError::Hook(
                        "ui_client_press event type is invalid".into(),
                    ));
                }
            };
            let event = pick_projection_fields(request, "/event", fields)?;
            result
                .set_field("event", event)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            result
        }
        "ui_press" => {
            let mut result = pick_projection_fields(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "handle",
                    "key",
                    "surface",
                    "href",
                    "client_id",
                ],
            )?;
            if request.get_property("surface").is_none() {
                result
                    .set_field("surface", plain_text_projection("desktop"))
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            result
        }
        "ui_input" => {
            let mut result = pick_projection_fields(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "handle",
                    "kind",
                    "value",
                    "key",
                    "component",
                    "instance_id",
                    "surface",
                    "client_id",
                ],
            )?;
            if request.get_property("surface").is_none() {
                result
                    .set_field("surface", plain_text_projection("desktop"))
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            result
        }
        "ui_select" => {
            let mut result = pick_projection_fields(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "handle",
                    "value",
                    "key",
                    "component",
                    "instance_id",
                    "surface",
                    "client_id",
                ],
            )?;
            if request.get_property("surface").is_none() {
                result
                    .set_field("surface", plain_text_projection("desktop"))
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            result
        }
        _ => return Err(ModError::Hook("unknown Mod UI control subtype".into())),
    };
    validate_control_request(&normalized)?;
    Ok(normalized)
}

fn pick_projection_fields(
    source: &Utf16JsonProjection,
    pointer: &str,
    fields: &[&str],
) -> Result<Utf16JsonProjection, ModError> {
    let object = source
        .at_pointer(pointer)
        .filter(|value| value.object_entries().is_some())
        .ok_or_else(|| ModError::Hook("Mod UI request value must be an object".into()))?;
    let mut selected = Vec::new();
    for field in fields {
        if object.get_property(field).is_some() {
            let path = if pointer.is_empty() {
                format!("/{field}")
            } else {
                format!("{pointer}/{field}")
            };
            selected.push((
                (*field).to_owned(),
                source
                    .subprojection(&path)
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ));
        }
    }
    Utf16JsonProjection::object(selected).map_err(|error| ModError::Protocol(error.to_string()))
}

pub(super) fn validate_control_request(request: &Utf16JsonProjection) -> Result<(), ModError> {
    request
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let subtype = request
        .string_at("/subtype")
        .ok_or_else(|| ModError::Hook("Mod UI control request needs a subtype".into()))?;
    match subtype {
        "ui_render" => validate_ui_render(request),
        "ui_client_module" => {
            exact_projection_keys(request, "", &["subtype", "plugin"])?;
            bounded_projection_string(request, "/plugin", "plugin", MAX_CONTROL_UTF16, false)?;
            Ok(())
        }
        "ui_message" => {
            exact_projection_keys(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "data",
                ],
            )?;
            validate_client_address(request)?;
            validate_plain_json_projection(
                request,
                "/data",
                MAX_CLIENT_MESSAGE_DEPTH,
                MAX_CLIENT_MESSAGE_VALUES,
                MAX_CLIENT_MESSAGE_UTF16,
            )
        }
        "ui_client_press" => {
            exact_projection_keys(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "element",
                    "event",
                ],
            )?;
            validate_client_address(request)?;
            bounded_projection_string(request, "/element", "element", MAX_CONTROL_UTF16, false)?;
            let event_type = request
                .string_at("/event/type")
                .ok_or_else(|| ModError::Hook("ui_client_press event needs a type".into()))?;
            match event_type {
                "press" => exact_projection_keys(request, "/event", &["type"]),
                "input" => {
                    exact_projection_keys(request, "/event", &["type", "kind", "value"])?;
                    if !matches!(request.string_at("/event/kind"), Some("change" | "submit")) {
                        return Err(ModError::Hook(
                            "ui_client_press input kind is invalid".into(),
                        ));
                    }
                    bounded_projection_string(
                        request,
                        "/event/value",
                        "value",
                        MAX_PRESS_VALUE_UTF16,
                        true,
                    )?;
                    Ok(())
                }
                "select" => {
                    exact_projection_keys(request, "/event", &["type", "value"])?;
                    bounded_projection_string(
                        request,
                        "/event/value",
                        "value",
                        MAX_PRESS_VALUE_UTF16,
                        true,
                    )?;
                    Ok(())
                }
                _ => Err(ModError::Hook(
                    "ui_client_press event type is invalid".into(),
                )),
            }
        }
        "ui_press" => validate_parent_ui_press(request),
        "ui_input" => validate_parent_ui_input(request),
        "ui_select" => validate_parent_ui_select(request),
        "ui_client_fault" => {
            exact_projection_keys(
                request,
                "",
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "phase",
                    "reason",
                ],
            )?;
            validate_client_address(request)?;
            UiClientFaultRequest::try_from_internal(request)
                .map(|_| ())
                .map_err(|error| ModError::Hook(error.to_string()))
        }
        _ => Err(ModError::Hook("unknown Mod UI control subtype".into())),
    }
}

fn validate_ui_render(request: &Utf16JsonProjection) -> Result<(), ModError> {
    exact_projection_keys(
        request,
        "",
        &[
            "subtype",
            "surface",
            "component",
            "instance_id",
            "props",
            "client_id",
            "viewport",
            "on_screen",
            "content_rows",
            "keyed",
            "bench",
        ],
    )?;
    if !matches!(
        request.string_at("/surface"),
        Some("desktop" | "mobile" | "vscode")
    ) {
        return Err(ModError::Hook("ui_render surface is invalid".into()));
    }
    component_projection(request, "/component")?;
    bounded_projection_string(
        request,
        "/instance_id",
        "instance_id",
        MAX_CONTROL_UTF16,
        false,
    )?;
    if request
        .at_pointer("/props")
        .and_then(|value| value.object_entries())
        .is_none()
    {
        return Err(ModError::Hook("ui_render props must be an object".into()));
    }
    validate_plain_json_projection(request, "/props", 32, 20_000, 100_000)?;
    if request.get_property("client_id").is_some() {
        let units = bounded_projection_string(request, "/client_id", "client_id", 64, false)?;
        if !units.iter().all(|unit| {
            *unit <= 0x7f
                && ((*unit as u8).is_ascii_alphanumeric()
                    || matches!(*unit as u8, b'.' | b'_' | b'-'))
        }) {
            return Err(ModError::Hook("ui_render client_id is invalid".into()));
        }
    }
    if request.get_property("viewport").is_some() {
        exact_projection_keys(request, "/viewport", &["columns", "rows", "isFullscreen"])?;
        positive_projection_integer(request, "/viewport/columns", "columns")?;
        positive_projection_integer(request, "/viewport/rows", "rows")?;
        if request.get_property("viewport").is_some()
            && request.at_pointer("/viewport/isFullscreen").is_some()
            && request
                .at_pointer("/viewport/isFullscreen")
                .and_then(|value| value.as_bool())
                .is_none()
        {
            return Err(ModError::Hook(
                "ui_render viewport isFullscreen must be boolean".into(),
            ));
        }
    }
    if request.get_property("on_screen").is_some()
        && !request
            .at_pointer("/on_screen")
            .is_some_and(|value| value.is_null())
    {
        exact_projection_keys(request, "/on_screen", &["first", "last", "of"])?;
        let first = nonnegative_projection_integer(request, "/on_screen/first", "first")?;
        let last = nonnegative_projection_integer(request, "/on_screen/last", "last")?;
        let of = positive_projection_integer(request, "/on_screen/of", "of")?;
        if first > last || last >= of {
            return Err(ModError::Hook(
                "ui_render on_screen range is invalid".into(),
            ));
        }
    }
    if request.get_property("content_rows").is_some() {
        nonnegative_projection_integer(request, "/content_rows", "content_rows")?;
    }
    if request.get_property("keyed").is_some() {
        let length = request
            .array_len("/keyed")
            .filter(|length| *length <= 512)
            .ok_or_else(|| {
                ModError::Hook("ui_render keyed must contain at most 512 rows".into())
            })?;
        for index in 0..length {
            let row = format!("/keyed/{index}");
            exact_projection_keys(request, &row, &["plugin", "key", "top", "bottom"])?;
            bounded_projection_string(
                request,
                &format!("{row}/plugin"),
                "plugin",
                MAX_CONTROL_UTF16,
                false,
            )?;
            bounded_projection_string(
                request,
                &format!("{row}/key"),
                "key",
                MAX_CONTROL_UTF16,
                false,
            )?;
            let top = nonnegative_projection_integer(request, &format!("{row}/top"), "top")?;
            let bottom =
                nonnegative_projection_integer(request, &format!("{row}/bottom"), "bottom")?;
            if bottom < top {
                return Err(ModError::Hook(
                    "ui_render keyed row range is invalid".into(),
                ));
            }
        }
    }
    if request.get_property("bench").is_some() {
        exact_projection_keys(request, "/bench", &["seq", "t0"])?;
        nonnegative_projection_integer(request, "/bench/seq", "seq")?;
        if !request.number_at("/bench/t0").is_some_and(f64::is_finite) {
            return Err(ModError::Hook("ui_render bench t0 must be finite".into()));
        }
    }
    Ok(())
}

fn validate_client_address(request: &Utf16JsonProjection) -> Result<(), ModError> {
    bounded_projection_string(request, "/plugin", "plugin", MAX_CONTROL_UTF16, false)?;
    component_projection(request, "/component")?;
    bounded_projection_string(
        request,
        "/instance_id",
        "instance_id",
        MAX_CONTROL_UTF16,
        false,
    )?;
    let key = request
        .string_units("/client")
        .ok_or_else(|| ModError::Hook("Mod UI request needs string field client".into()))?;
    if key.is_empty() || key.len() > MAX_CONTROL_UTF16 {
        return Err(ModError::Hook(
            "Mod UI field client exceeds its UTF-16 limit".into(),
        ));
    }
    bounded_projection_string(request, "/module", "module", MAX_CONTROL_UTF16, false)?;
    Ok(())
}

fn validate_parent_ui_press(request: &Utf16JsonProjection) -> Result<(), ModError> {
    exact_projection_keys(
        request,
        "",
        &[
            "subtype",
            "plugin",
            "handle",
            "key",
            "surface",
            "href",
            "client_id",
        ],
    )?;
    bounded_projection_string(request, "/plugin", "plugin", MAX_CONTROL_UTF16, false)?;
    positive_projection_integer(request, "/handle", "handle")?;
    if request.get_property("key").is_some() {
        projection_string_units(request, "/key", "key")?;
    }
    parent_surface_projection(request)?;
    if request.get_property("href").is_some() {
        bounded_projection_string(request, "/href", "href", 2_048, true)?;
    }
    validate_optional_client_id_projection(request)
}

fn validate_parent_ui_input(request: &Utf16JsonProjection) -> Result<(), ModError> {
    exact_projection_keys(
        request,
        "",
        &[
            "subtype",
            "plugin",
            "handle",
            "kind",
            "value",
            "key",
            "component",
            "instance_id",
            "surface",
            "client_id",
        ],
    )?;
    bounded_projection_string(request, "/plugin", "plugin", MAX_CONTROL_UTF16, false)?;
    positive_projection_integer(request, "/handle", "handle")?;
    if !matches!(request.string_at("/kind"), Some("change" | "submit")) {
        return Err(ModError::Hook(
            "ui_input kind must be change or submit".into(),
        ));
    }
    bounded_projection_string(request, "/value", "value", MAX_PRESS_VALUE_UTF16, true)?;
    validate_optional_parent_address_projection(request)?;
    parent_surface_projection(request)?;
    validate_optional_client_id_projection(request)
}

fn validate_parent_ui_select(request: &Utf16JsonProjection) -> Result<(), ModError> {
    exact_projection_keys(
        request,
        "",
        &[
            "subtype",
            "plugin",
            "handle",
            "value",
            "key",
            "component",
            "instance_id",
            "surface",
            "client_id",
        ],
    )?;
    bounded_projection_string(request, "/plugin", "plugin", MAX_CONTROL_UTF16, false)?;
    positive_projection_integer(request, "/handle", "handle")?;
    bounded_projection_string(request, "/value", "value", MAX_PRESS_VALUE_UTF16, true)?;
    validate_optional_parent_address_projection(request)?;
    parent_surface_projection(request)?;
    validate_optional_client_id_projection(request)
}

fn validate_optional_parent_address_projection(
    request: &Utf16JsonProjection,
) -> Result<(), ModError> {
    if request.get_property("key").is_some() {
        projection_string_units(request, "/key", "key")?;
    }
    if request.get_property("component").is_some() {
        component_projection(request, "/component")?;
    }
    if request.get_property("instance_id").is_some() {
        bounded_projection_string(
            request,
            "/instance_id",
            "instance_id",
            MAX_CONTROL_UTF16,
            true,
        )?;
    }
    Ok(())
}

fn parent_surface_projection(request: &Utf16JsonProjection) -> Result<&str, ModError> {
    match request.string_at("/surface") {
        Some(surface @ ("desktop" | "mobile" | "vscode")) => Ok(surface),
        _ => Err(ModError::Hook("parent UI surface is invalid".into())),
    }
}

fn component_projection(
    request: &Utf16JsonProjection,
    pointer: &str,
) -> Result<ClientUiComponent, ModError> {
    request
        .string_at(pointer)
        .and_then(ClientUiComponent::parse)
        .ok_or_else(|| ModError::Hook("Mod UI component is invalid".into()))
}

fn exact_projection_keys(
    request: &Utf16JsonProjection,
    pointer: &str,
    allowed: &[&str],
) -> Result<(), ModError> {
    let entries = request
        .at_pointer(pointer)
        .and_then(|value| value.object_entries())
        .ok_or_else(|| ModError::Hook("Mod UI value must be an object".into()))?;
    if entries
        .iter()
        .any(|entry| !allowed.contains(&entry.display_key))
    {
        return Err(ModError::Hook("Mod UI value has unknown fields".into()));
    }
    Ok(())
}

fn projection_string_units(
    request: &Utf16JsonProjection,
    pointer: &str,
    field: &str,
) -> Result<Vec<u16>, ModError> {
    request
        .string_units(pointer)
        .ok_or_else(|| ModError::Hook(format!("{field} must be a string")))
}

fn bounded_projection_string(
    request: &Utf16JsonProjection,
    pointer: &str,
    field: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<Vec<u16>, ModError> {
    let units = projection_string_units(request, pointer, field)?;
    if (!allow_empty && units.is_empty()) || units.len() > maximum {
        return Err(ModError::Hook(format!("{field} exceeds its UTF-16 limit")));
    }
    Ok(units)
}

fn projection_integer(
    request: &Utf16JsonProjection,
    pointer: &str,
    field: &str,
    positive: bool,
) -> Result<u64, ModError> {
    let Some(value) = request.number_at(pointer) else {
        return Err(ModError::Hook(format!("{field} must be an integer")));
    };
    if !value.is_finite()
        || value.fract() != 0.0
        || value < if positive { 1.0 } else { 0.0 }
        || value > MAX_JS_SAFE_INTEGER as f64
    {
        return Err(ModError::Hook(format!(
            "{field} must be a {}non-negative safe integer",
            if positive { "positive " } else { "" }
        )));
    }
    Ok(value as u64)
}

fn nonnegative_projection_integer(
    request: &Utf16JsonProjection,
    pointer: &str,
    field: &str,
) -> Result<u64, ModError> {
    projection_integer(request, pointer, field, false)
}

fn positive_projection_integer(
    request: &Utf16JsonProjection,
    pointer: &str,
    field: &str,
) -> Result<u64, ModError> {
    projection_integer(request, pointer, field, true)
}

fn validate_optional_client_id_projection(request: &Utf16JsonProjection) -> Result<(), ModError> {
    if request.get_property("client_id").is_none() {
        return Ok(());
    }
    let units = bounded_projection_string(request, "/client_id", "client_id", 64, false)?;
    if !units.iter().all(|unit| {
        *unit <= 0x7f
            && ((*unit as u8).is_ascii_alphanumeric() || matches!(*unit as u8, b'.' | b'_' | b'-'))
    }) {
        return Err(ModError::Hook("client_id is invalid".into()));
    }
    Ok(())
}

fn validate_client_operation(operation: &Utf16JsonProjection) -> Result<(), ModError> {
    operation
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let kind = operation
        .string_at("/type")
        .ok_or_else(|| ModError::Hook("UI Client operation needs a type".into()))?;
    match kind {
        "mount" => {
            exact_projection_keys(
                operation,
                "",
                &[
                    "type",
                    "surface",
                    "component",
                    "instance_id",
                    "plugin",
                    "client",
                    "module",
                    "render_revision",
                    "columns",
                    "rows",
                ],
            )?;
            if operation.string_at("/surface") != Some("desktop") {
                return Err(ModError::Hook(
                    "Client mount surface must be desktop".into(),
                ));
            }
            component_projection(operation, "/component")?;
            bounded_projection_string(
                operation,
                "/instance_id",
                "instance_id",
                MAX_CONTROL_UTF16,
                false,
            )?;
            bounded_projection_string(operation, "/plugin", "plugin", MAX_CONTROL_UTF16, false)?;
            let client = bounded_projection_string(
                operation,
                "/client",
                "client",
                MAX_PARENT_CLIENT_UTF16,
                false,
            )?;
            if client.is_empty() {
                return Err(ModError::Hook("mount client must not be empty".into()));
            }
            bounded_projection_string(
                operation,
                "/module",
                "module",
                MAX_PARENT_CLIENT_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
            positive_projection_integer(operation, "/columns", "columns")?;
            positive_projection_integer(operation, "/rows", "rows")?;
        }
        "render" | "unmount" => {
            exact_projection_keys(operation, "", &["type", "runtimeId", "render_revision"])?;
            bounded_projection_string(
                operation,
                "/runtimeId",
                "runtimeId",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
        }
        "setProps" => {
            exact_projection_keys(
                operation,
                "",
                &["type", "runtimeId", "render_revision", "props"],
            )?;
            bounded_projection_string(
                operation,
                "/runtimeId",
                "runtimeId",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
            if operation
                .at_pointer("/props")
                .and_then(|value| value.object_entries())
                .is_none()
            {
                return Err(ModError::Hook("Client props must be an object".into()));
            }
            validate_plain_json_projection(operation, "/props", 32, 20_000, 100_000)?;
        }
        "resize" => {
            exact_projection_keys(
                operation,
                "",
                &["type", "runtimeId", "render_revision", "columns", "rows"],
            )?;
            bounded_projection_string(
                operation,
                "/runtimeId",
                "runtimeId",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
            positive_projection_integer(operation, "/columns", "columns")?;
            positive_projection_integer(operation, "/rows", "rows")?;
        }
        "pointer" | "key" => {
            exact_projection_keys(
                operation,
                "",
                &["type", "runtimeId", "render_revision", "event"],
            )?;
            bounded_projection_string(
                operation,
                "/runtimeId",
                "runtimeId",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
            validate_plain_json_projection(operation, "/event", 32, 20_000, 100_000)?;
        }
        "runHeld" => {
            exact_projection_keys(
                operation,
                "",
                &["type", "runtimeId", "render_revision", "event", "handle"],
            )?;
            bounded_projection_string(
                operation,
                "/runtimeId",
                "runtimeId",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
            positive_projection_integer(operation, "/handle", "handle")?;
            if operation.get_property("event").is_some() {
                validate_plain_json_projection(operation, "/event", 32, 20_000, 100_000)?;
            }
        }
        "draw_commit" => {
            exact_projection_keys(
                operation,
                "",
                &[
                    "type",
                    "surface",
                    "component",
                    "instance_id",
                    "render_revision",
                    "clients",
                ],
            )?;
            if operation.string_at("/surface") != Some("desktop") {
                return Err(ModError::Hook(
                    "Client draw_commit surface must be desktop".into(),
                ));
            }
            component_projection(operation, "/component")?;
            bounded_projection_string(
                operation,
                "/instance_id",
                "instance_id",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
            let length = operation
                .array_len("/clients")
                .filter(|length| *length <= 20_000)
                .ok_or_else(|| {
                    ModError::Hook("Client draw_commit clients must be a bounded array".into())
                })?;
            for index in 0..length {
                let client = format!("/clients/{index}");
                exact_projection_keys(operation, &client, &["plugin", "key", "module"])?;
                bounded_projection_string(
                    operation,
                    &format!("{client}/plugin"),
                    "plugin",
                    MAX_CONTROL_UTF16,
                    false,
                )?;
                bounded_projection_string(
                    operation,
                    &format!("{client}/key"),
                    "key",
                    MAX_PARENT_CLIENT_UTF16,
                    false,
                )?;
                bounded_projection_string(
                    operation,
                    &format!("{client}/module"),
                    "module",
                    MAX_PARENT_CLIENT_UTF16,
                    false,
                )?;
            }
        }
        "draw_unmount" => {
            exact_projection_keys(
                operation,
                "",
                &[
                    "type",
                    "surface",
                    "component",
                    "instance_id",
                    "render_revision",
                ],
            )?;
            if operation.string_at("/surface") != Some("desktop") {
                return Err(ModError::Hook(
                    "Client draw_unmount surface must be desktop".into(),
                ));
            }
            component_projection(operation, "/component")?;
            bounded_projection_string(
                operation,
                "/instance_id",
                "instance_id",
                MAX_CONTROL_UTF16,
                false,
            )?;
            positive_projection_integer(operation, "/render_revision", "render_revision")?;
        }
        _ => return Err(ModError::Hook("unknown Client operation type".into())),
    }
    Ok(())
}

fn validate_plain_json_projection(
    projection: &Utf16JsonProjection,
    pointer: &str,
    maximum_depth: usize,
    maximum_values: usize,
    maximum_utf16: usize,
) -> Result<(), ModError> {
    let value = if pointer.is_empty() {
        projection.clone()
    } else {
        projection
            .subprojection(pointer)
            .map_err(|error| ModError::Protocol(error.to_string()))?
    };
    let mut pending = vec![(
        value
            .at_pointer("")
            .ok_or_else(|| ModError::Protocol("invalid flat JSON projection".into()))?,
        0usize,
    )];
    let mut count = 0usize;
    while let Some((current, depth)) = pending.pop() {
        if depth > maximum_depth {
            return Err(ModError::Hook(
                "Mod UI data exceeds its nesting limit".into(),
            ));
        }
        count = count.saturating_add(1);
        if count > maximum_values {
            return Err(ModError::Hook("Mod UI data exceeds its value limit".into()));
        }
        if let Some(length) = current.array_len() {
            for index in 0..length {
                let child = current.get_index(index).ok_or_else(|| {
                    ModError::Protocol("invalid flat JSON array projection".into())
                })?;
                pending.push((child, depth + 1));
            }
        } else if let Some(entries) = current.object_entries() {
            pending.extend(entries.into_iter().map(|entry| (entry.value, depth + 1)));
        }
    }
    let encoded = value
        .to_json_string()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    if encoded.encode_utf16().count() > maximum_utf16 {
        return Err(ModError::Hook(
            "Mod UI data exceeds its serialized character limit".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn client_site_from_input(input: &Value) -> Result<ClientUiRenderSite, ModError> {
    let projection = Utf16JsonProjection::from_serde_value(input)
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    client_site_from_projection(&projection)
}

fn client_identity_from_control(input: &Utf16JsonProjection) -> Result<ClientUiIdentity, ModError> {
    let key = input
        .string_units("/client")
        .ok_or_else(|| ModError::Hook("Mod UI request needs string field client".into()))?;
    let plugin = ExactHookText::from_json_projection(input, "/plugin")
        .filter(|plugin| !plugin.is_empty() && plugin.len_utf16() <= MAX_CONTROL_UTF16)
        .ok_or_else(|| ModError::Hook("Mod UI request needs a valid plugin string".into()))?;
    let module = ExactHookText::from_json_projection(input, "/module")
        .filter(|module| !module.is_empty() && module.len_utf16() <= MAX_CONTROL_UTF16)
        .ok_or_else(|| ModError::Hook("Mod UI request needs a valid module string".into()))?;
    ClientUiIdentity::new(plugin, module, key).map_err(|error| ModError::Hook(error.to_string()))
}

fn client_site_from_projection(
    input: &Utf16JsonProjection,
) -> Result<ClientUiRenderSite, ModError> {
    let component = input
        .string_at("/component")
        .and_then(ClientUiComponent::parse)
        .ok_or_else(|| ModError::Hook("Mod UI request needs a valid component".into()))?;
    let request_id = ExactHookText::from_json_projection(input, "/instance_id")
        .filter(|request_id| !request_id.is_empty())
        .ok_or_else(|| ModError::Hook("Mod UI request needs a non-empty instance_id".into()))?;
    ClientUiRenderSite::new(component, request_id)
        .map_err(|error| ModError::Hook(error.to_string()))
}

fn parent_control_site_from_projection(
    input: &Utf16JsonProjection,
) -> Result<ParentControlSite, ModError> {
    let surface = input
        .string_at("/surface")
        .filter(|surface| matches!(*surface, "desktop" | "mobile" | "vscode" | "terminal"))
        .ok_or_else(|| ModError::Hook("Mod UI request needs a valid surface".into()))?;
    let component = input
        .string_at("/component")
        .and_then(ClientUiComponent::parse)
        .ok_or_else(|| ModError::Hook("Mod UI request needs a valid component".into()))?;
    let request_id = input
        .string_units("/instance_id")
        .filter(|request_id| !request_id.is_empty() && request_id.len() <= MAX_CONTROL_UTF16)
        .ok_or_else(|| ModError::Hook("Mod UI request needs a valid instance_id".into()))?;
    Ok(ParentControlSite {
        surface: surface.to_owned(),
        component,
        request_id,
    })
}

fn pointer_child(parent: &str, token: &str) -> String {
    let token = token.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{token}")
}

fn exact_string_projection(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display is derived from exact UTF-16 units")
}

fn client_frame_identity_projections(
    key: &ClientUiInstanceKey,
) -> (Utf16JsonProjection, Utf16JsonProjection) {
    let parent = Utf16JsonProjection::object(vec![
        ("surface".into(), plain_text_projection("desktop")),
        (
            "component".into(),
            plain_text_projection(key.site.component().as_str()),
        ),
        (
            "instance_id".into(),
            exact_text_projection(key.site.request_id()),
        ),
    ])
    .expect("Client frame parent identity is an object");
    let client = Utf16JsonProjection::object(vec![
        (
            "plugin".into(),
            exact_text_projection(key.identity.plugin()),
        ),
        (
            "key".into(),
            exact_string_projection(key.identity.key_units().to_vec()),
        ),
        (
            "module".into(),
            exact_text_projection(key.identity.module()),
        ),
    ])
    .expect("Client frame identity is an object");
    (parent, client)
}

#[cfg(test)]
fn set_exact_string_field(
    projection: &mut Utf16JsonProjection,
    field: &str,
    units: Vec<u16>,
) -> Result<(), ModError> {
    projection
        .set_field(field, exact_string_projection(units))
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn collect_client_parents(
    projection: &Utf16JsonProjection,
    clients: &mut HashMap<ClientUiIdentity, Utf16JsonProjection>,
    seen_plugin_keys: &mut HashSet<(Vec<u16>, Vec<u16>)>,
) -> Result<(), ModError> {
    fn visit(
        projection: &Utf16JsonProjection,
        pointer: &str,
        clients: &mut HashMap<ClientUiIdentity, Utf16JsonProjection>,
        seen_plugin_keys: &mut HashSet<(Vec<u16>, Vec<u16>)>,
    ) -> Result<(), ModError> {
        let node = projection
            .at_pointer(pointer)
            .ok_or_else(|| ModError::Protocol("invalid UI tree projection".into()))?;
        if let Some(length) = node.array_len() {
            for index in 0..length {
                visit(
                    projection,
                    &pointer_child(pointer, &index.to_string()),
                    clients,
                    seen_plugin_keys,
                )?;
            }
            return Ok(());
        }
        if node.get_property("type").and_then(|value| value.as_str()) == Some("Client") {
            let client_node = projection
                .subprojection(pointer)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            super::ui::source::validate_client_parent_element(&client_node)
                .map_err(ModError::Hook)?;
            let plugin = ExactHookText::from_json_projection(
                projection,
                &pointer_child(&pointer_child(pointer, "client"), "plugin"),
            )
            .filter(|plugin| !plugin.is_empty())
            .ok_or_else(|| ModError::Hook("Client parent has no trusted owner".into()))?;
            let props_pointer = pointer_child(pointer, "props");
            let key_pointer = pointer_child(&props_pointer, "key");
            let key = projection
                .string_units(&key_pointer)
                .ok_or_else(|| ModError::Hook("Client parent key is not a string".into()))?;
            let module = ExactHookText::from_json_projection(
                projection,
                &pointer_child(&props_pointer, "module"),
            )
            .ok_or_else(|| ModError::Hook("Client parent module is not a string".into()))?;
            if !seen_plugin_keys.insert((plugin.utf16_code_units.clone(), key.clone())) {
                return Err(ModError::Hook(
                    "ui.render draws a Client key more than once for one plugin".into(),
                ));
            }
            let identity = ClientUiIdentity::new(plugin, module, key)
                .map_err(|error| ModError::Hook(error.to_string()))?;
            let client_props_pointer = pointer_child(&props_pointer, "props");
            let client_props = if projection.at_pointer(&client_props_pointer).is_some() {
                projection
                    .subprojection(&client_props_pointer)
                    .map_err(|error| ModError::Protocol(error.to_string()))?
            } else {
                Utf16JsonProjection::object(Vec::new())
                    .map_err(|error| ModError::Protocol(error.to_string()))?
            };
            validate_plain_json_projection(&client_props, "", 32, 20_000, 100_000)?;
            clients.insert(identity, client_props);
            return Ok(());
        }
        if node.get_property("children").is_some() {
            visit(
                projection,
                &pointer_child(pointer, "children"),
                clients,
                seen_plugin_keys,
            )?;
        }
        Ok(())
    }

    visit(projection, "", clients, seen_plugin_keys)
}

fn collect_parent_press_actions(
    tree: &Utf16JsonProjection,
    site: &ParentControlSite,
) -> Result<Vec<ParentUiPressAction>, ModError> {
    fn visit(
        tree: &Utf16JsonProjection,
        pointer: &str,
        site: &ParentControlSite,
        actions: &mut HashMap<(Vec<u16>, u64), ParentUiPressAction>,
    ) -> Result<(), ModError> {
        let node = tree
            .at_pointer(pointer)
            .ok_or_else(|| ModError::Protocol("invalid parent UI projection".into()))?;
        if let Some(length) = node.array_len() {
            for index in 0..length {
                visit(
                    tree,
                    &pointer_child(pointer, &index.to_string()),
                    site,
                    actions,
                )?;
            }
            return Ok(());
        }
        let Some(_object) = node.object_entries() else {
            return Ok(());
        };
        let kind = node
            .get_property("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if matches!(kind, "Button" | "Input" | "Select" | "Markdown") {
            let press_pointer = pointer_child(pointer, "press");
            if tree.at_pointer(&press_pointer).is_some() {
                let token = tree
                    .at_pointer(&press_pointer)
                    .and_then(|value| value.object_entries())
                    .ok_or_else(|| {
                        ModError::Protocol("parent UI press token is not an object".into())
                    })?;
                if token
                    .iter()
                    .any(|entry| !matches!(entry.display_key, "plugin" | "handle"))
                {
                    return Err(ModError::Protocol(
                        "parent UI press token has unknown fields".into(),
                    ));
                }
                let plugin = tree
                    .string_units(&pointer_child(&press_pointer, "plugin"))
                    .filter(|plugin| !plugin.is_empty())
                    .ok_or_else(|| {
                        ModError::Protocol("parent UI press token has no plugin".into())
                    })?;
                let handle = tree
                    .number_at(&pointer_child(&press_pointer, "handle"))
                    .filter(|handle| {
                        handle.is_finite()
                            && handle.fract() == 0.0
                            && *handle > 0.0
                            && *handle <= MAX_JS_SAFE_INTEGER as f64
                    })
                    .map(|handle| handle as u64)
                    .ok_or_else(|| {
                        ModError::Protocol("parent UI press token has an invalid handle".into())
                    })?;
                let props_pointer = pointer_child(pointer, "props");
                if tree
                    .at_pointer(&props_pointer)
                    .and_then(|value| value.object_entries())
                    .is_none()
                {
                    return Err(ModError::Protocol(
                        "pressable parent UI element has no props".into(),
                    ));
                }
                let element = tree.string_units(&pointer_child(&props_pointer, "key"));
                if element.as_deref().is_none_or(<[u16]>::is_empty) {
                    return Err(ModError::Protocol(
                        "parent held-action element has no key".into(),
                    ));
                }
                let select_values = if kind == "Select" {
                    let options_pointer = pointer_child(&props_pointer, "options");
                    tree.array_len(&options_pointer)
                        .map(|length| {
                            (0..length)
                                .filter_map(|index| {
                                    tree.string_units(&pointer_child(
                                        &pointer_child(&options_pointer, &index.to_string()),
                                        "value",
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    HashSet::new()
                };
                let pressable_links = if kind == "Markdown" {
                    let links_pointer = pointer_child(&props_pointer, "pressableLinks");
                    tree.array_len(&links_pointer)
                        .map(|length| {
                            (0..length)
                                .map(|index| {
                                    tree.string_units(&pointer_child(
                                        &links_pointer,
                                        &index.to_string(),
                                    ))
                                    .ok_or_else(|| {
                                        ModError::Protocol(
                                            "Markdown pressable link is not a string".into(),
                                        )
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .transpose()?
                } else {
                    None
                };
                let action = ParentUiPressAction {
                    site: site.clone(),
                    surface: site.surface.clone(),
                    plugin: plugin.clone(),
                    handle,
                    element,
                    kind: kind.to_owned(),
                    pressable_links,
                    select_values,
                };
                if let Some(previous) = actions.insert((plugin, handle), action.clone()) {
                    if previous.site != action.site
                        || previous.element != action.element
                        || previous.kind != action.kind
                        || previous.pressable_links != action.pressable_links
                        || previous.select_values != action.select_values
                    {
                        return Err(ModError::Protocol(
                            "parent UI tree repeats a press handle for different elements".into(),
                        ));
                    }
                }
            }
        }
        if node.get_property("children").is_some() {
            visit(tree, &pointer_child(pointer, "children"), site, actions)?;
        }
        Ok(())
    }

    let mut actions = HashMap::new();
    visit(tree, "", site, &mut actions)?;
    Ok(actions.into_values().collect())
}

impl ModHost {
    pub(super) async fn invalidate_unloaded_client_sites(
        &self,
        sites: &[ClientUiRenderSite],
        session: &dyn ModSessionContext,
    ) {
        if sites.is_empty() {
            return;
        }
        let sequence = self
            .client_ui
            .next_invalidation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let uuid = format!("mod-ui-{}-{sequence}", self.instance_id);
        let instances = Utf16JsonProjection::array(
            sites
                .iter()
                .map(|site| {
                    Utf16JsonProjection::object(vec![
                        ("surface".into(), plain_text_projection("desktop")),
                        (
                            "component".into(),
                            plain_text_projection(site.component().as_str()),
                        ),
                        (
                            "instance_id".into(),
                            exact_text_projection(site.request_id()),
                        ),
                    ])
                    .expect("invalidation fields form a JSON object")
                })
                .collect(),
        )
        .expect("invalidation instances form a JSON array");
        let session_id = session.id().await;
        session
            .emit_mod_ui_invalidate(Some(&instances), &uuid, &session_id)
            .await;
    }

    /// Dispatch one canonical Native `ui_*` control request for this session.
    pub async fn dispatch_client_ui_control(
        &self,
        request: Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<ModUiControlOutcome, ModError> {
        let request = normalize_control_request(&request)?;
        let subtype = request.string_at("/subtype").expect("validated subtype");
        let (response, render_revision, client_runtime_epochs, client_state_token) = match subtype {
            "ui_client_module" => (
                self.client_module_manifest(&request)?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_render" => {
                let outcome = self.dispatch_parent_ui_render(&request, session).await?;
                (
                    outcome.response,
                    outcome.render_revision,
                    outcome.client_runtime_epochs,
                    outcome.client_state_token,
                )
            }
            "ui_message" => (
                self.dispatch_client_message(&request, session, None).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_client_press" => (
                self.dispatch_client_press(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_client_fault" => (
                self.dispatch_client_fault(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_press" => (
                self.dispatch_parent_ui_press(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_input" => (
                self.dispatch_parent_ui_input(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_select" => (
                self.dispatch_parent_ui_select(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            _ => unreachable!("canonical request validator accepted an unknown subtype"),
        };
        Ok(ModUiControlOutcome {
            response,
            render_revision,
            client_runtime_epochs,
            client_state_token,
        })
    }

    /// Execute a host-local Client VM operation. Renderer data never supplies
    /// module source, a graph, a manifest, or the props used for first mount.
    pub async fn dispatch_client_ui_operation(
        &self,
        operation: Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        validate_client_operation(&operation)?;
        let kind = operation.string_at("/type").expect("validated type");
        let revision = operation
            .number_at("/render_revision")
            .filter(|revision| revision.is_finite() && revision.fract() == 0.0 && *revision >= 0.0)
            .map(|revision| revision as u64)
            .expect("validated render revision");
        match kind {
            "draw_commit" => self.commit_client_draw(&operation, revision, session).await,
            "draw_unmount" => self.unmount_client_site(&operation, revision).await,
            "mount" => {
                self.mount_client_from_parent(&operation, revision, session)
                    .await
            }
            _ => self.dispatch_runtime_operation(&operation, revision).await,
        }
    }

    fn client_module_manifest(
        &self,
        request: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let plugin_identity = ExactHookText::from_json_projection(request, "/plugin")
            .filter(|plugin| !plugin.is_empty())
            .ok_or_else(|| ModError::Hook("ui_client_module needs a plugin".into()))?;
        let plugin = host_plugin_name(&plugin_identity).ok_or_else(|| {
            ModError::Hook("ui_client_module plugin is not valid host text".into())
        })?;
        let prepared = lock(&self.client_ui.data).modules.get(&plugin).cloned();
        match prepared {
            Some(prepared) => {
                let manifest = prepared
                    .for_plugin(&plugin)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                Utf16JsonProjection::from_serde_value(&manifest)
                    .map_err(|error| ModError::Protocol(error.to_string()))
            }
            None => Err(ModError::Hook(format!(
                "ui_client_module: plugin {plugin} is not loaded or its hooks module names no surface module"
            ))),
        }
    }

    async fn dispatch_parent_ui_render(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<ModUiControlOutcome, ModError> {
        let surface = request.string_at("/surface").expect("validated surface");
        let component = request
            .string_at("/component")
            .expect("validated component");
        let instance_id = ExactHookText::from_json_projection(request, "/instance_id")
            .expect("validated instance_id");
        let props = request
            .subprojection("/props")
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let on_screen = request
            .at_pointer("/on_screen")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let render_pace = ModUiRenderPace::for_site(surface, component, on_screen);
        let revision = self.client_ui.next_revision()?;
        let control_site = parent_control_site_from_projection(request)?;
        {
            let mut data = lock(&self.client_ui.data);
            data.parent_control_latest
                .insert(control_site.clone(), revision);
        }
        let site = if surface == "desktop" {
            Some(client_site_from_projection(request)?)
        } else {
            None
        };
        let token = site.as_ref().map(|site| {
            let state_version = self.current_client_ui_render_state_version(
                "desktop",
                site.component().as_str(),
                &site.request_id().utf16_code_units,
            );
            self.client_ui
                .fault_registry
                .begin_render(site.clone(), state_version)
        });
        if let (Some(site), Some(token)) = (site.as_ref(), token) {
            let mut data = lock(&self.client_ui.data);
            data.latest_started.insert(site.clone(), revision);
            data.pending_tokens.insert((site.clone(), revision), token);
        }

        let mut event = Utf16JsonProjection::object(vec![
            ("surface".into(), plain_text_projection(surface)),
            ("component".into(), plain_text_projection(component)),
            ("requestId".into(), exact_text_projection(&instance_id)),
            ("props".into(), props.clone()),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        for (wire, hook) in [
            ("client_id", "clientId"),
            ("on_screen", "onScreen"),
            ("content_rows", "contentRows"),
        ] {
            if request.get_property(wire).is_some() {
                event
                    .set_field(
                        hook,
                        request
                            .subprojection(&format!("/{wire}"))
                            .map_err(|error| ModError::Protocol(error.to_string()))?,
                    )
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
        }
        for field in ["viewport", "keyed", "bench"] {
            if request.get_property(field).is_some() {
                event
                    .set_field(
                        field,
                        request
                            .subprojection(&format!("/{field}"))
                            .map_err(|error| ModError::Protocol(error.to_string()))?,
                    )
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
        }
        let event_projection =
            super::ui::source::normalize_ui_render_event(&event).map_err(ModError::Hook)?;

        let engine = host_generated_json(json!({"type":"engine","ref":revision}));
        let engine_for_core = engine.clone();
        let final_props = std::sync::Arc::new(StdMutex::new(props.clone()));
        let captured_props = final_props.clone();
        let dispatch_result = self
            .dispatch_ui_render_with_state_scope_at_session(
                event_projection,
                revision,
                session,
                move |forwarded| {
                    *lock(&captured_props) = forwarded
                        .as_ref()
                        .and_then(|forwarded| forwarded.subprojection("/props").ok())
                        .unwrap_or_else(|| host_generated_json(json!(null)));
                    let result = engine_for_core.clone();
                    async move { Ok(Some(result)) }
                },
                |plugin, text| async move { session.emit_mod_log(&plugin, text).await },
                |plugin, text, timeout_ms| async move {
                    session.emit_mod_toast(&plugin, text, timeout_ms).await
                },
                |plugin, text| async move { session.emit_mod_status(&plugin, text).await },
            )
            .await;
        let (tree, render_plugins) = match dispatch_result {
            Ok(outcome) => (outcome.result_projection, outcome.ui_render_plugins),
            Err(error) => {
                if let Some(site) = site.as_ref() {
                    lock(&self.client_ui.data)
                        .pending_tokens
                        .remove(&(site.clone(), revision));
                }
                self.discard_client_ui_render_state(
                    surface,
                    component,
                    &instance_id.utf16_code_units,
                    revision,
                );
                return Err(error);
            }
        };
        let normalized_tree_is_invalid = tree.validate().is_err()
            || (surface == "desktop"
                && tree.string_at("/type") != Some("engine")
                && super::ui::source::validate_desktop_parent_tree(&tree).is_err());
        let tree = if normalized_tree_is_invalid {
            engine.clone()
        } else {
            tree
        };
        let resolved_props = if normalized_tree_is_invalid {
            props.clone()
        } else {
            lock(&final_props).clone()
        };

        let mut clients = HashMap::new();
        let mut client_modules = Vec::new();
        let mut client_runtime_epochs = BTreeMap::new();
        let press_actions = collect_parent_press_actions(&tree, &control_site)?;
        if surface == "desktop" {
            collect_client_parents(&tree, &mut clients, &mut HashSet::new())?;
            let data = lock(&self.client_ui.data);
            for identity in clients.keys() {
                let Some(plugin) = host_plugin_name(identity.plugin()) else {
                    continue;
                };
                if let Some(prepared) = data.modules.get(&plugin) {
                    if prepared.manifest.modules.iter().any(|module| {
                        module.module.encode_utf16().eq(identity
                            .module()
                            .utf16_code_units
                            .iter()
                            .copied())
                    }) {
                        client_modules.push((
                            plugin.clone(),
                            Utf16JsonProjection::root_string(
                                prepared.manifest.hash.clone(),
                                prepared.manifest.hash.encode_utf16().collect(),
                            )
                            .expect("manifest hash is valid UTF-16"),
                        ));
                        if let Some(epoch) = data.client_runtime_epochs.get(&plugin) {
                            client_runtime_epochs.insert(plugin, *epoch);
                        }
                    }
                }
            }
        }

        let mut client_state_token = None;
        if let Some(site) = site {
            let has_client_modules = !client_modules.is_empty();
            let has_clients = !clients.is_empty() && has_client_modules;
            let (is_latest, invalidation_update) = {
                let mut data = lock(&self.client_ui.data);
                let is_latest = data.latest_started.get(&site) == Some(&revision);
                data.pending_tokens.remove(&(site.clone(), revision));
                if is_latest {
                    let (state_token, update) = self.finish_client_ui_render_state(
                        surface,
                        component,
                        &instance_id.utf16_code_units,
                        revision,
                        render_plugins,
                        has_clients,
                        render_pace,
                    );
                    client_state_token = state_token.clone();
                    let version = state_token
                        .as_deref()
                        .and_then(|token| token.parse::<u64>().ok())
                        .unwrap_or(0);
                    let token = self
                        .client_ui
                        .fault_registry
                        .begin_render(site.clone(), version);
                    data.parents.insert(
                        site.clone(),
                        ParentRender {
                            revision,
                            token: Some(token),
                            committed_generation: None,
                            clients,
                        },
                    );
                    (true, update)
                } else {
                    (false, ModUiInvalidationUpdate::default())
                }
            };
            if is_latest {
                self.apply_ui_invalidation_update(invalidation_update, Some(session))
                    .await;
            } else {
                self.discard_client_ui_render_state(
                    surface,
                    component,
                    &instance_id.utf16_code_units,
                    revision,
                );
            }
        }
        {
            let mut data = lock(&self.client_ui.data);
            if data.parent_control_latest.get(&control_site) == Some(&revision) {
                data.parent_control_renders.insert(
                    control_site,
                    ParentControlRender {
                        revision,
                        press_actions,
                    },
                );
            }
        }

        let rewritten = tree != engine || resolved_props != props;
        let mut response = host_generated_json(json!({
            "rewritten": rewritten,
            "hooked": self.has_event("ui.render"),
        }));
        response
            .set_field("tree", tree)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        response
            .set_field("props", resolved_props)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        if !client_modules.is_empty() {
            let client_modules = Utf16JsonProjection::object(client_modules.into_iter().collect())
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            response
                .set_field("client_modules", client_modules)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        if request.get_property("bench").is_some() {
            response
                .set_field(
                    "bench",
                    request
                        .subprojection("/bench")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        Ok(ModUiControlOutcome {
            response,
            render_revision: Some(revision),
            client_runtime_epochs,
            client_state_token,
        })
    }

    async fn dispatch_client_press(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_projection(request)?;
        let identity = client_identity_from_control(request)?;
        if !self.client_is_currently_drawn(&site, &identity) {
            return Ok(host_generated_json(json!({"handled":false})));
        }
        let plugin = host_plugin_name(identity.plugin()).ok_or_else(|| {
            ModError::Hook("Client plugin identity is not valid host text".into())
        })?;
        let surface = "desktop";
        let component = site.component().as_str();
        let (hook_event, include_value) = match request.string_at("/event/type") {
            Some("press") => ("ui.press", false),
            Some("input") => ("ui.input", true),
            Some("select") => ("ui.select", true),
            _ => unreachable!("press request was validated"),
        };
        let mut input = Utf16JsonProjection::object(vec![
            ("plugin".into(), exact_text_projection(identity.plugin())),
            (
                "element".into(),
                request
                    .subprojection("/element")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ),
            ("component".into(), plain_text_projection(component)),
            ("requestId".into(), exact_text_projection(site.request_id())),
            ("surface".into(), plain_text_projection(surface)),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        if hook_event == "ui.input" {
            input
                .set_field(
                    "kind",
                    request
                        .subprojection("/event/kind")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        if include_value {
            input
                .set_field(
                    "value",
                    request
                        .subprojection("/event/value")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        let reached = std::sync::Arc::new(StdMutex::new(None::<Utf16JsonProjection>));
        let reached_core = reached.clone();
        let plugin_for_dispatch = plugin.clone();
        // Client control admission is based on the exact currently drawn
        // Client identity. Keep that authorization distinct from the parent
        // UI's private { plugin, handle } action token.
        let client_action_context = Utf16JsonProjection::object(vec![
            ("kind".into(), plain_text_projection("client-ui-action")),
            ("plugin".into(), exact_text_projection(identity.plugin())),
            (
                "clientKey".into(),
                exact_string_projection(identity.key_units().to_vec()),
            ),
            ("component".into(), plain_text_projection(component)),
            ("requestId".into(), exact_text_projection(site.request_id())),
            ("surface".into(), plain_text_projection(surface)),
            (
                "element".into(),
                request
                    .subprojection("/element")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ),
            ("event".into(), plain_text_projection(hook_event)),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let result = self
            .dispatch_plugin_scoped_ui_event(
                hook_event,
                &plugin_for_dispatch,
                input,
                session,
                Some(client_action_context),
                None,
                move |forwarded| {
                    *lock(&reached_core) = forwarded.clone();
                    let mut response = host_generated_json(json!({"handled":true}));
                    if let Some(forwarded) = forwarded.as_ref() {
                        if let Ok(element) = forwarded.subprojection("/element") {
                            response
                                .set_field("element", element)
                                .expect("element field belongs to object");
                        }
                        if include_value {
                            let value = forwarded
                                .subprojection("/value")
                                .expect("validated input has a value");
                            response
                                .set_field("value", value)
                                .expect("value field belongs to object");
                        }
                    }
                    async move { Ok(Some(response)) }
                },
            )
            .await?;
        let handled = result
            .at_pointer("/handled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let mut response = host_generated_json(json!({"handled":handled}));
        if let Some(reached) = lock(&reached).as_ref() {
            let mut value = host_generated_json(json!({}));
            if let Ok(element) = reached.subprojection("/element") {
                value
                    .set_field("element", element)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            if include_value {
                if let Ok(field_value) = reached.subprojection("/value") {
                    value
                        .set_field("value", field_value)
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
            }
            response
                .set_field("reached", value)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        Ok(response)
    }

    async fn dispatch_parent_ui_press(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let href = request
            .string_units("/href")
            .map(|units| String::from_utf16(&units))
            .transpose()
            .map_err(|_| ModError::Hook("ui.press href is not valid host text".into()))?;
        let expected_kind = if href.is_some() { "Markdown" } else { "Button" };
        self.dispatch_parent_held_action(
            request,
            "ui.press",
            Some(expected_kind),
            href.as_deref(),
            session,
        )
        .await
    }

    async fn dispatch_parent_ui_input(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        self.dispatch_parent_held_action(request, "ui.input", Some("Input"), None, session)
            .await
    }

    async fn dispatch_parent_ui_select(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        self.dispatch_parent_held_action(request, "ui.select", Some("Select"), None, session)
            .await
    }

    async fn dispatch_parent_held_action(
        &self,
        request: &Utf16JsonProjection,
        event_name: &'static str,
        expected_kind: Option<&str>,
        href: Option<&str>,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let plugin = request
            .string_units("/plugin")
            .filter(|plugin| !plugin.is_empty())
            .ok_or_else(|| ModError::Hook("ui action has no plugin identity".into()))?;
        let surface = request.string_at("/surface").unwrap_or("desktop");
        let handle = request
            .number_at("/handle")
            .filter(|handle| {
                handle.is_finite()
                    && handle.fract() == 0.0
                    && *handle > 0.0
                    && *handle <= MAX_JS_SAFE_INTEGER as f64
            })
            .map(|handle| handle as u64);
        let action = handle.and_then(|handle| {
            self.current_parent_press_action(
                &plugin,
                handle,
                surface,
                request.string_units("/key").as_deref(),
                request
                    .string_at("/component")
                    .and_then(ClientUiComponent::parse),
                request.string_units("/instance_id").as_deref(),
                expected_kind,
            )
        });
        let Some(action) = action else {
            return Ok(host_generated_json(json!({"handled":false})));
        };
        if event_name == "ui.press" && action.surface != "desktop" {
            return Ok(host_generated_json(json!({"handled":false})));
        }
        let mut press_href_admitted = false;
        if event_name == "ui.press" {
            if let Some(href) = href {
                if let Some(allowed) = action.pressable_links.as_ref() {
                    let href_units = href.encode_utf16().collect::<Vec<_>>();
                    let allowed_links = allowed
                        .iter()
                        .map(|link| String::from_utf16(link))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            ModError::Hook("parent Markdown link is not valid host text".into())
                        })?;
                    if !allowed.contains(&href_units) {
                        return Ok(host_generated_json(json!({"handled":false})));
                    }
                    if !self
                        .preflight_parent_press_href(href, &allowed_links, &session.cwd())
                        .await?
                    {
                        return Ok(host_generated_json(json!({"handled":false})));
                    }
                }
                // The capability stays in the private Host→worker dispatch
                // envelope. It is stripped before the plugin sees the event.
                press_href_admitted = true;
            }
            if href.is_some() && action.kind != "Markdown" {
                return Ok(host_generated_json(json!({"handled":false})));
            }
        }
        let value = if event_name == "ui.press" {
            None
        } else {
            request.string_units("/value")
        };
        if event_name == "ui.select"
            && !action
                .select_values
                .contains(&value.expect("select has value"))
        {
            return Ok(host_generated_json(json!({"handled":false})));
        }

        let plugin_text = ExactHookText::from_utf16(action.plugin.clone());
        let host_plugin = host_plugin_name(&plugin_text).ok_or_else(|| {
            ModError::Hook("parent action plugin identity is not valid host text".into())
        })?;
        let mut fields = vec![
            ("plugin".into(), exact_text_projection(&plugin_text)),
            (
                "component".into(),
                plain_text_projection(action.site.component.as_str()),
            ),
            (
                "requestId".into(),
                exact_string_projection(action.site.request_id.clone()),
            ),
            ("surface".into(), plain_text_projection(&action.surface)),
        ];
        if let Some(element) = action.element.as_ref() {
            fields.push(("element".into(), exact_string_projection(element.clone())));
        }
        if event_name == "ui.input" {
            fields.push((
                "kind".into(),
                request
                    .subprojection("/kind")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ));
        }
        if href.is_some() {
            fields.push((
                "link".into(),
                request
                    .subprojection("/href")
                    .and_then(|href| Utf16JsonProjection::object(vec![("href".into(), href)]))
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ));
        }
        if event_name != "ui.press" {
            fields.push((
                "value".into(),
                request
                    .subprojection("/value")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ));
        }
        let input = Utf16JsonProjection::object(fields)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let mut press_token =
            host_generated_json(json!({"plugin":host_plugin,"handle":action.handle}));
        if press_href_admitted {
            press_token
                .set_field("hostPressHrefAdmitted", host_generated_json(json!(true)))
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        let reached = std::sync::Arc::new(StdMutex::new(None::<Utf16JsonProjection>));
        let reached_core = reached.clone();
        let include_value = event_name != "ui.press";
        let plugin_for_dispatch = host_plugin;
        let result = self
            .dispatch_plugin_scoped_ui_event(
                event_name,
                &plugin_for_dispatch,
                input,
                session,
                Some(press_token),
                None,
                move |forwarded| {
                    *lock(&reached_core) = forwarded.clone();
                    let mut response = host_generated_json(json!({"handled":true}));
                    if let Some(forwarded) = forwarded.as_ref() {
                        if let Ok(element) = forwarded.subprojection("/element") {
                            response
                                .set_field("element", element)
                                .expect("element belongs to response object");
                        }
                        if include_value {
                            let value = forwarded
                                .subprojection("/value")
                                .expect("validated input has value");
                            response
                                .set_field("value", value)
                                .expect("value belongs to response object");
                        }
                    }
                    async move { Ok(Some(response)) }
                },
            )
            .await?;
        let handled = result
            .at_pointer("/handled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let mut response = host_generated_json(json!({"handled":handled}));
        if handled {
            if let Some(forwarded) = lock(&reached).as_ref() {
                if let Ok(element) = forwarded.subprojection("/element") {
                    response
                        .set_field("element", element)
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
                if include_value {
                    if let Ok(value) = forwarded.subprojection("/value") {
                        response
                            .set_field("value", value)
                            .map_err(|error| ModError::Protocol(error.to_string()))?;
                    }
                }
            }
        }
        Ok(response)
    }

    async fn preflight_parent_press_href(
        &self,
        href: &str,
        pressable_links: &[String],
        cwd: &Path,
    ) -> Result<bool, ModError> {
        let cwd = cwd
            .to_str()
            .ok_or_else(|| ModError::Protocol("session cwd is not valid UTF-8".into()))?;
        let mut request = self
            .request_projection(host_generated_json(json!({
                "kind":"ui.press.preflight",
                "href":href,
                "pressableLinks":pressable_links,
                "cwd":cwd,
            })))
            .await?;
        let reply = self.receive(&mut request).await?;
        if reply.string_at("/kind") != Some("ui.press.preflight.result") {
            return Err(ModError::Protocol(format!(
                "unexpected UI press preflight reply: {}",
                reply.to_json_string().unwrap_or_default()
            )));
        }
        reply
            .at_pointer("/admitted")
            .and_then(|value| value.as_bool())
            .ok_or_else(|| ModError::Protocol("UI press preflight has no admission result".into()))
    }

    async fn dispatch_client_message(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
        post_action_id: Option<&str>,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_projection(request)?;
        let identity = client_identity_from_control(request)?;
        let Some(instance) = self.current_instance(&site, &identity) else {
            return Ok(host_generated_json(json!({"handled":false})));
        };
        if !self.client_is_currently_drawn(&site, &identity) {
            return Ok(host_generated_json(json!({"handled":false})));
        }
        let input = Utf16JsonProjection::object(vec![
            ("surface".into(), plain_text_projection("desktop")),
            (
                "component".into(),
                plain_text_projection(site.component().as_str()),
            ),
            ("requestId".into(), exact_text_projection(site.request_id())),
            (
                "element".into(),
                exact_string_projection(identity.key_units().to_vec()),
            ),
            ("module".into(), exact_text_projection(identity.module())),
            (
                "data".into(),
                request
                    .subprojection("/data")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let plugin = host_plugin_name(identity.plugin()).ok_or_else(|| {
            ModError::Hook("Client plugin identity is not valid host text".into())
        })?;
        let result = self
            .dispatch_plugin_scoped_ui_event(
                "ui.message",
                &plugin,
                input,
                session,
                None,
                post_action_id,
                |_| async { Ok(Some(host_generated_json(json!({"handled":true})))) },
            )
            .await?;
        let handled = result
            .at_pointer("/handled")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        let mut response = host_generated_json(json!({"handled":handled}));
        if result
            .at_pointer("/props")
            .and_then(|value| value.object_entries())
            .is_some()
        {
            let props = result
                .subprojection("/props")
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            let mut operation = host_generated_json(json!({
                "type":"setProps",
                "runtimeId":instance.runtime_id.clone(),
                "generation":instance.worker_generation,
            }));
            operation
                .set_field("props", props.clone())
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            if let Some(post_action_id) = post_action_id {
                operation.set_field("postActionId", plain_text_projection(post_action_id))
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            let frame = self.run_client_worker_operation(operation).await?;
            self.update_instance_props(&instance.runtime_id, props.clone());
            let frame = self
                .frame_for_worker_result(&instance.runtime_id, instance.revision, &plugin, &frame)
                .ok_or_else(|| {
                    ModError::Unavailable("Client message produced no current frame".into())
                })?;
            self.emit_client_frame(session, &instance.runtime_id, instance.revision, &frame)
                .await?;
            response
                .set_field("props", props)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        Ok(response)
    }

    async fn dispatch_client_fault(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let fault = UiClientFaultRequest::try_from_internal(request)
            .map_err(|error| ModError::Hook(error.to_string()))?;
        let session_id = session.id().await;
        let adapter = ClientFaultHostAdapter {
            host: self,
            session,
            session_id,
        };
        let outcome =
            dispatch_client_ui_fault(&self.client_ui.fault_registry, &fault, &adapter).await?;
        Ok(host_generated_json(json!({
            "handled":!matches!(outcome, crate::mod_ui_fault::ClientUiFaultOutcome::NotDrawn)
        })))
    }

    async fn dispatch_mount_client_fault(
        &self,
        site: &ClientUiRenderSite,
        identity: &ClientUiIdentity,
        phase: &str,
        reason: &ExactHookText,
        session: &dyn ModSessionContext,
    ) {
        let reason = crate::mod_ui_fault::normalize_client_fault_reason(
            identity.plugin(),
            identity.module(),
            reason,
        );
        let request = match Utf16JsonProjection::object(vec![
            ("subtype".into(), plain_text_projection("ui_client_fault")),
            ("plugin".into(), exact_text_projection(identity.plugin())),
            (
                "component".into(),
                plain_text_projection(site.component().as_str()),
            ),
            (
                "instance_id".into(),
                exact_text_projection(site.request_id()),
            ),
            (
                "client".into(),
                exact_string_projection(identity.key_units().to_vec()),
            ),
            ("module".into(), exact_text_projection(identity.module())),
            ("phase".into(), plain_text_projection(phase)),
            ("reason".into(), exact_text_projection(&reason)),
        ]) {
            Ok(request) => request,
            Err(_) => return,
        };
        // The operation response carries the authoritative worker failure.
        // A hook failure while reporting the worker's async fault must not
        // replace that result with a second, transport-shaped failure.
        let _ = self.dispatch_client_fault(&request, session).await;
    }

    fn client_is_currently_drawn(
        &self,
        site: &ClientUiRenderSite,
        identity: &ClientUiIdentity,
    ) -> bool {
        let address =
            crate::mod_ui_fault::ClientUiFaultAddress::new(site.clone(), identity.clone());
        self.client_ui.fault_registry.is_drawn(&address)
    }

    fn current_parent_press_action(
        &self,
        plugin: &[u16],
        handle: u64,
        surface: &str,
        key: Option<&[u16]>,
        component: Option<ClientUiComponent>,
        request_id: Option<&[u16]>,
        expected_kind: Option<&str>,
    ) -> Option<ParentUiPressAction> {
        let data = lock(&self.client_ui.data);
        let mut matched = None;
        for (site, render) in &data.parent_control_renders {
            if data.parent_control_latest.get(site) != Some(&render.revision) {
                continue;
            }
            for action in &render.press_actions {
                if action.plugin.as_slice() != plugin
                    || action.handle != handle
                    || action.surface != surface
                    || key.is_some_and(|key| action.element.as_deref() != Some(key))
                    || component.is_some_and(|component| action.site.component != component)
                    || request_id
                        .is_some_and(|request_id| action.site.request_id.as_slice() != request_id)
                    || expected_kind.is_some_and(|kind| action.kind != kind)
                {
                    continue;
                }
                if matched.is_some() {
                    // Parent handles are worker-issued and normally unique.
                    // Refuse an ambiguous match instead of choosing a site.
                    return None;
                }
                matched = Some(action.clone());
            }
        }
        matched
    }

    fn current_instance(
        &self,
        site: &ClientUiRenderSite,
        identity: &ClientUiIdentity,
    ) -> Option<ClientUiInstance> {
        let data = lock(&self.client_ui.data);
        let key = ClientUiInstanceKey {
            site: site.clone(),
            identity: identity.clone(),
        };
        let runtime_id = data.instance_by_identity.get(&key)?;
        let instance = data.instances.get(runtime_id)?;
        (instance.mounted
            && instance.worker_fault.is_none()
            && instance.revision == data.parents.get(site)?.revision
            && instance.key.identity == *identity)
            .then(|| instance.clone())
    }

    fn client_instance_is_current(
        &self,
        data: &ClientUiHostData,
        instance: &ClientUiInstance,
    ) -> bool {
        data.instances
            .get(&instance.runtime_id)
            .is_some_and(|current| {
                current.key == instance.key
                    && current.revision == instance.revision
                    && current.worker_generation == instance.worker_generation
                    && current.mounted
                    && current.worker_fault.is_none()
            })
            && data
                .instance_by_identity
                .get(&instance.key)
                .map(String::as_str)
                == Some(instance.runtime_id.as_str())
            && data.parents.get(&instance.key.site).is_some_and(|parent| {
                parent.revision == instance.revision
                    && parent.clients.contains_key(&instance.key.identity)
            })
            && self.client_ui.fault_registry.is_drawn(
                &crate::mod_ui_fault::ClientUiFaultAddress::new(
                    instance.key.site.clone(),
                    instance.key.identity.clone(),
                ),
            )
    }

    fn update_instance_props(&self, runtime_id: &str, props: Utf16JsonProjection) {
        if let Some(instance) = lock(&self.client_ui.data).instances.get_mut(runtime_id) {
            instance.props = props;
        }
    }

    fn record_client_worker_fault(
        &self,
        runtime_id: &str,
        phase: &str,
        reason: &ExactHookText,
    ) -> bool {
        let mut data = lock(&self.client_ui.data);
        let Some(instance) = data.instances.get_mut(runtime_id) else {
            return false;
        };
        if instance.worker_fault.is_some() {
            return false;
        }
        instance.worker_fault = Some(ClientUiWorkerFault {
            phase: phase.to_owned(),
            reason: reason.clone(),
        });
        true
    }

    fn record_client_environment_fault(&self, owner: &ClientUiInstance, phase: &str, reason: &ExactHookText) {
        let ids = {
            let mut data = lock(&self.client_ui.data);
            let mut ids = Vec::new();
            for instance in data.instances.values_mut() {
                let same_environment = owner.worker_environment_id.as_ref().is_some_and(|id| {
                    instance.worker_environment_id.as_ref() == Some(id)
                });
                if same_environment || instance.runtime_id == owner.runtime_id {
                    if instance.worker_fault.is_none() {
                        instance.worker_fault = Some(ClientUiWorkerFault { phase: phase.into(), reason: reason.clone() });
                    }
                    ids.push(instance.runtime_id.clone());
                }
            }
            ids
        };
        for id in ids {
            self.client_ui.fail_post_actions_for_runtime(&id, "Client shared environment is unavailable");
        }
    }

    fn claim_client_worker_fault_dispatch(&self, runtime_id: &str) -> bool {
        let mut data = lock(&self.client_ui.data);
        let Some(instance) = data.instances.get_mut(runtime_id) else {
            return false;
        };
        if instance.worker_fault_event_dispatched {
            return false;
        }
        instance.worker_fault_event_dispatched = true;
        true
    }

    fn claim_client_worker_fault_snapshot(&self, runtime_id: &str) -> Option<ClientFaultSnapshotClaim<'_>> {
        let mut data = lock(&self.client_ui.data);
        let instance = data.instances.get_mut(runtime_id)?;
        if instance.worker_fault_snapshot != ClientFaultSnapshotState::Pending {
            return None;
        }
        instance.worker_fault_snapshot = ClientFaultSnapshotState::Publishing;
        Some(ClientFaultSnapshotClaim {
            state: &self.client_ui,
            runtime_id: runtime_id.to_owned(),
            committed: false,
        })
    }

    fn client_worker_fault(&self, runtime_id: &str) -> Option<ClientUiWorkerFault> {
        lock(&self.client_ui.data)
            .instances
            .get(runtime_id)?
            .worker_fault
            .clone()
    }

    async fn dispatch_plugin_scoped_ui_event<F, Fut>(
        &self,
        event: &str,
        plugin: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        press_token: Option<Utf16JsonProjection>,
        post_action_id: Option<&str>,
        core: F,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: std::future::Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
    {
        let cwd = session.cwd();
        let scope = ModDispatchScope {
            plugin_scope: Some(plugin.to_owned()),
            press_token,
            ui_render_host_revision: None,
            client_post_action: post_action_id.map(str::to_owned),
        };
        let outcome = self
            .dispatch_with_utf16_at_context_scope(
                event,
                input,
                &cwd,
                Some(session),
                scope,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                core,
                |plugin, text| async move { session.emit_mod_log(&plugin, text).await },
                |plugin, text, timeout_ms| async move {
                    session.emit_mod_toast(&plugin, text, timeout_ms).await
                },
                |plugin, text| async move { session.emit_mod_status(&plugin, text).await },
            )
            .await?;
        Ok(outcome.result_projection)
    }

    async fn has_matching_ui_listener(
        &self,
        plugin: &str,
        event: &str,
        input: &Utf16JsonProjection,
    ) -> Result<bool, ModError> {
        let mut envelope = host_generated_json(json!({
            "kind":"ui.listener",
            "plugin":plugin,
            "event":event,
        }));
        envelope
            .set_field("input", input.clone())
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let mut request = self.request_projection(envelope).await?;
        let reply = self.receive(&mut request).await?;
        match reply.string_at("/kind") {
            Some("ui.listener.result") => Ok(reply
                .at_pointer("/matched")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)),
            Some("error") => Err(ModError::Hook(
                ExactHookText::from_json_projection(&reply, "/message")
                    .map(|message| message.display)
                    .unwrap_or_else(|| "UI listener matcher failed".into()),
            )),
            _ => Err(ModError::Protocol(format!(
                "unexpected UI listener reply: {}",
                reply.to_json_string().unwrap_or_default()
            ))),
        }
    }

    async fn run_client_worker_operation(
        &self,
        operation: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ClientWorkerOperationError> {
        self.run_client_worker_operation_inner(operation, None)
            .await
            .map(|(result, _, _)| result)
    }

    async fn run_client_worker_operation_with_post_action(
        &self,
        operation: Utf16JsonProjection,
        post_action: &mut ClientPostActionGuard,
    ) -> Result<(Utf16JsonProjection, u64, Option<RequestChannel>), ClientWorkerOperationError> {
        self.run_client_worker_operation_inner(operation, Some(post_action))
            .await
    }

    async fn run_client_worker_operation_inner(
        &self,
        operation: Utf16JsonProjection,
        post_action: Option<&mut ClientPostActionGuard>,
    ) -> Result<(Utf16JsonProjection, u64, Option<RequestChannel>), ClientWorkerOperationError> {
        let mut envelope = host_generated_json(json!({
            "kind":"ui.client.operation",
        }));
        envelope
            .set_field("operation", operation)
            .map_err(|error| {
                ClientWorkerOperationError::Host(ModError::Protocol(error.to_string()))
            })?;
        let cancellation = post_action.as_ref().map(|action| action.cancellation.clone());
        let mut request = if let Some(post_action) = post_action {
            self.request_projection_for_client_post_action(envelope, post_action)
                .await?
        } else {
            self.request_projection(envelope).await?
        };
        let worker_epoch = request
            .worker_epoch
            .as_ref()
            .map(|epoch| epoch.id)
            .ok_or_else(|| {
                ClientWorkerOperationError::Host(ModError::Protocol(
                    "Client operation has no worker epoch".into(),
                ))
            })?;
        let reply = if let Some(cancellation) = cancellation.as_ref() {
            // Dropping the request sends cancel to the Worker, preventing a
            // suspended action from continuing to publish after owner loss.
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ClientWorkerOperationError::UnknownFault {
                    phase: "run".into(),
                    reason: ExactHookText::from_text("Client runHeld was cancelled with outcome unknown"),
                }),
                reply = self.receive(&mut request) => reply,
            }
        } else {
            self.receive(&mut request).await
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error @ ModError::Timeout) if cancellation.is_some() => {
                return Err(ClientWorkerOperationError::UnknownFault {
                    phase: "run".into(),
                    reason: ExactHookText::from_text(&error.to_string()),
                });
            }
            Err(ModError::Unavailable(message)) if cancellation.is_some() && message == "worker exited" => {
                return Err(ClientWorkerOperationError::UnknownFault {
                    phase: "run".into(), reason: ExactHookText::from_text(&message),
                });
            }
            Err(error) => return Err(error.into()),
        };
        match reply.string_at("/kind") {
            Some("ui.client.result") => {
                if reply.get_property("error").is_some() {
                    if reply.at_pointer("/error/busy").and_then(|value| value.as_bool()) == Some(true) {
                        return Err(ClientWorkerOperationError::Host(ModError::Unavailable(
                            "Client shared environment is busy; operation was not executed".into(),
                        )));
                    }
                    let phase = reply
                        .string_at("/error/phase")
                        .filter(|phase| matches!(*phase, "load" | "render" | "run"))
                        .ok_or_else(|| {
                            ClientWorkerOperationError::Host(ModError::Protocol(
                                "Client worker fault has an invalid phase".into(),
                            ))
                        })?;
                    let reason = ExactHookText::from_json_projection(&reply, "/error/reason")
                        .ok_or_else(|| {
                            ClientWorkerOperationError::Host(ModError::Protocol(
                                "Client worker fault has no reason".into(),
                            ))
                        })?;
                    if reply.at_pointer("/error/outcomeUnknown").and_then(|value| value.as_bool()) == Some(true) {
                        return Err(ClientWorkerOperationError::UnknownFault {
                            phase: phase.to_owned(), reason,
                        });
                    }
                    return Err(ClientWorkerOperationError::Fault {
                        phase: phase.to_owned(),
                        reason,
                    });
                }
                let result = reply.subprojection("/result").map_err(|_| {
                    ClientWorkerOperationError::Host(ModError::Protocol(
                        "Client operation reply has no result".into(),
                    ))
                })?;
                let held_request = if cancellation.is_some()
                    && result.at_pointer("/accepted").and_then(|value| value.as_bool()) == Some(true)
                {
                    // ui.client.result is provisional; Drop must still send
                    // cancellation until a finalization acknowledgement.
                    request.completed = false;
                    #[cfg(test)]
                    self.client_ui.held_result_received.fetch_add(1, Ordering::SeqCst);
                    Some(request)
                } else { None };
                Ok((result, worker_epoch, held_request))
            }
            Some("error") => Err(ClientWorkerOperationError::Host(ModError::Hook(
                reply
                    .string_at("/message")
                    .unwrap_or("Client operation failed")
                    .into(),
            ))),
            _ => Err(ClientWorkerOperationError::Host(ModError::Protocol(
                format!(
                    "unexpected Client operation reply: {}",
                    reply.to_json_string().unwrap_or_default()
                ),
            ))),
        }
    }

    fn frame_for_worker_result(
        &self,
        runtime_id: &str,
        revision: u64,
        plugin: &str,
        result: &Utf16JsonProjection,
    ) -> Option<Utf16JsonProjection> {
        if result
            .at_pointer("/stale")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            return None;
        }
        let frame_sequence = result.number_at("/frameSequence").filter(|sequence| {
            sequence.is_finite()
                && sequence.fract() == 0.0
                && *sequence > 0.0
                && *sequence <= MAX_JS_SAFE_INTEGER as f64
        })? as u64;
        let has_pointer_listener = result.at_pointer("/hasPointerListener")?.as_bool()?;
        let has_key_listener = result.at_pointer("/hasKeyListener")?.as_bool()?;
        let mut frame = host_generated_json(json!({
            "runtimeId":runtime_id,
            "renderRevision":revision,
            "frameSequence":frame_sequence,
            "hasPointerListener":has_pointer_listener,
            "hasKeyListener":has_key_listener,
        }));
        let worker_tree = result.subprojection("/tree").ok()?;
        let public_tree = public_client_tree_projection(&worker_tree, plugin)?;
        frame.set_field("tree", public_tree).ok()?;
        Some(frame)
    }

    async fn emit_client_frame(
        &self,
        session: &dyn ModSessionContext,
        runtime_id: &str,
        revision: u64,
        frame: &Utf16JsonProjection,
    ) -> Result<(), ModError> {
        let Some(instance_key) = lock(&self.client_ui.data)
            .instances
            .get(runtime_id)
            .map(|instance| instance.key.clone())
        else {
            return Err(ModError::Unavailable(
                "Client runtime was removed before its frame was delivered".into(),
            ));
        };
        let (Ok(frame_sequence), Ok(has_pointer_listener), Ok(has_key_listener)) = (
            frame.subprojection("/frameSequence"),
            frame.subprojection("/hasPointerListener"),
            frame.subprojection("/hasKeyListener"),
        ) else {
            return Err(ModError::Protocol(
                "Client frame metadata is missing".into(),
            ));
        };
        let mut public = match Utf16JsonProjection::object(vec![
            (
                "renderRevision".into(),
                Utf16JsonProjection::parse(&revision.to_string()).expect("revision is valid JSON"),
            ),
            ("frameSequence".into(), frame_sequence),
            ("hasPointerListener".into(), has_pointer_listener),
            ("hasKeyListener".into(), has_key_listener),
        ]) {
            Ok(public) => public,
            Err(error) => return Err(ModError::Protocol(error.to_string())),
        };
        let tree = frame
            .subprojection("/tree")
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        public
            .set_field("tree", tree)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let (parent, client) = client_frame_identity_projections(&instance_key);
        public
            .set_field("parent", parent)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        public
            .set_field("client", client)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        session.emit_mod_ui_client_frame(runtime_id, &public).await
    }

    async fn mount_client_from_parent(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_projection(operation)?;
        let plugin_identity =
            ExactHookText::from_json_projection(operation, "/plugin").expect("validated plugin");
        let plugin = host_plugin_name(&plugin_identity).ok_or_else(|| {
            ModError::Hook("Client plugin identity is not valid host text".into())
        })?;
        let module_identity =
            ExactHookText::from_json_projection(operation, "/module").expect("validated module");
        let _module = String::from_utf16(module_identity.utf16_code_units.as_slice())
            .map_err(|_| ModError::Hook("Client module identity is not valid host text".into()))?;
        let key_units = operation.string_units("/client").expect("validated client");
        let identity = ClientUiIdentity::new(
            plugin_identity.clone(),
            module_identity.clone(),
            key_units.clone(),
        )
        .map_err(|error| ModError::Hook(error.to_string()))?;
        let identity_key = ClientUiInstanceKey {
            site: site.clone(),
            identity: identity.clone(),
        };
        let (trusted_props, prepared) = {
            let data = lock(&self.client_ui.data);
            let Some(parent) = data
                .parents
                .get(&site)
                .filter(|parent| parent.revision == revision)
            else {
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            let Some(props) = parent.clients.get(&identity).cloned() else {
                return Err(ModError::Hook(
                    "Client mount is not in the latest validated parent tree".into(),
                ));
            };
            let Some(prepared) = data.modules.get(&plugin).cloned() else {
                return Err(ModError::Unavailable(format!(
                    "{plugin}: Client module source is unavailable"
                )));
            };
            if !prepared.manifest.modules.iter().any(|candidate| {
                candidate
                    .module
                    .encode_utf16()
                    .eq(module_identity.utf16_code_units.iter().copied())
            }) {
                return Err(ModError::Hook(format!(
                    "{plugin}: Client module is not in its prepared manifest"
                )));
            }
            (props, prepared)
        };
        let module_graph = prepared
            .worker_module_graph()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let runtime_id = self.client_ui.next_runtime_id(self.instance_id);
        let old_instance = {
            let mut data = lock(&self.client_ui.data);
            if data.latest_started.get(&site) != Some(&revision) {
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            }
            let old_runtime = data.instance_by_identity.remove(&identity_key);
            if let Some(old_runtime) = old_runtime.as_ref() {
                data.instances.remove(old_runtime)
            } else {
                None
            }
        };
        if let Some(old_instance) = old_instance {
            self.client_ui.fail_post_actions_for_runtime(
                &old_instance.runtime_id,
                "Client runtime was replaced",
            );
            let mut unmount =
                host_generated_json(json!({"type":"unmount","runtimeId":old_instance.runtime_id}));
            if let Some(generation) = old_instance.worker_generation {
                unmount
                    .set_field(
                        "generation",
                        Utf16JsonProjection::parse(&generation.to_string())
                            .expect("generation is valid JSON"),
                    )
                    .expect("worker generation belongs to unmount operation");
            }
            let _ = self.run_client_worker_operation(unmount).await;
        }

        let manifest_hash = prepared.manifest.hash.clone();
        let instance = ClientUiInstance {
            key: identity_key.clone(),
            runtime_id: runtime_id.clone(),
            revision,
            worker_generation: None,
            worker_environment_id: None,
            pending_events: Vec::new(),
            draining_pending_events: false,
            props: trusted_props.clone(),
            mounted: false,
            worker_fault: None,
            worker_fault_snapshot: ClientFaultSnapshotState::Pending,
            worker_fault_event_dispatched: false,
        };
        {
            let mut data = lock(&self.client_ui.data);
            data.instances.insert(runtime_id.clone(), instance);
            data.instance_by_identity
                .insert(identity_key, runtime_id.clone());
        }

        let mut child_request_id_units = site.request_id().utf16_code_units.clone();
        child_request_id_units.extend([
            0,
            u16::from(b'c'),
            u16::from(b'l'),
            u16::from(b'i'),
            u16::from(b'e'),
            u16::from(b'n'),
            u16::from(b't'),
            0,
        ]);
        child_request_id_units.extend_from_slice(&key_units);
        let parent = Utf16JsonProjection::object(vec![
            ("surface".into(), plain_text_projection("desktop")),
            (
                "component".into(),
                plain_text_projection(site.component().as_str()),
            ),
            (
                "requestId".into(),
                exact_string_projection(child_request_id_units),
            ),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let client = Utf16JsonProjection::object(vec![
            ("key".into(), exact_string_projection(key_units)),
            ("module".into(), exact_text_projection(&module_identity)),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let module_graph = Utf16JsonProjection::from_serde_value(&module_graph)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let worker_operation = Utf16JsonProjection::object(vec![
            ("type".into(), plain_text_projection("mount")),
            ("plugin".into(), exact_text_projection(&plugin_identity)),
            (
                "environmentId".into(),
                plain_text_projection(&manifest_hash),
            ),
            ("manifestHash".into(), plain_text_projection(&manifest_hash)),
            ("runtimeId".into(), plain_text_projection(&runtime_id)),
            ("parent".into(), parent),
            ("client".into(), client),
            ("moduleGraph".into(), module_graph),
            ("props".into(), trusted_props.clone()),
            (
                "columns".into(),
                operation
                    .subprojection("/columns")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ),
            (
                "rows".into(),
                operation
                    .subprojection("/rows")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            ),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let result = match self.run_client_worker_operation(worker_operation).await {
            Ok(result) => result,
            Err(error) => {
                let instance_key = ClientUiInstanceKey {
                    site: site.clone(),
                    identity: identity.clone(),
                };
                {
                    let mut data = lock(&self.client_ui.data);
                    self.client_ui.remove_instance_if_current(
                        &mut data,
                        &runtime_id,
                        &instance_key,
                    );
                }
                match error {
                    ClientWorkerOperationError::Fault { phase, reason } => {
                        self.dispatch_mount_client_fault(
                            &site, &identity, &phase, &reason, session,
                        )
                        .await;
                        return Ok(worker_fault_response(
                            Some(&runtime_id),
                            revision,
                            identity.plugin(),
                            identity.module(),
                            &phase,
                            &reason,
                        ));
                    }
                    error => return Err(error.into()),
                }
            }
        };
        let worker_generation = result
            .number_at("/generation")
            .filter(|generation| {
                generation.is_finite()
                    && generation.fract() == 0.0
                    && *generation > 0.0
                    && *generation <= MAX_JS_SAFE_INTEGER as f64
            })
            .map(|generation| generation as u64)
            .ok_or_else(|| {
                ModError::Protocol("Client mount result has no worker generation".into())
            })?;
        let worker_environment_id = result.string_at("/environmentInstanceId")
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or_else(|| ModError::Protocol("Client mount result has no worker environment identity".into()))?
            .to_owned();
        let still_current = {
            let mut data = lock(&self.client_ui.data);
            if data.latest_started.get(&site) != Some(&revision)
                || data
                    .parents
                    .get(&site)
                    .is_none_or(|parent| parent.revision != revision)
            {
                if let Some(instance) = data.instances.get(&runtime_id) {
                    let key = instance.key.clone();
                    self.client_ui
                        .remove_instance_if_current(&mut data, &runtime_id, &key);
                }
                false
            } else if let Some(instance) = data.instances.get_mut(&runtime_id) {
                instance.worker_generation = Some(worker_generation);
                instance.worker_environment_id = Some(worker_environment_id);
                instance.mounted = true;
                true
            } else {
                false
            }
        };
        if !still_current {
            let _ = self
                .run_client_worker_operation(host_generated_json(
                    json!({"type":"unmount","runtimeId":runtime_id,"generation":worker_generation}),
                ))
                .await;
            return Ok(host_generated_json(
                json!({"handled":false,"renderRevision":revision}),
            ));
        }
        self.frame_for_worker_result(&runtime_id, revision, &plugin, &result)
            .ok_or_else(|| ModError::Protocol("Client mount result has no render frame".into()))
    }

    async fn dispatch_runtime_operation(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
    ) -> Result<Utf16JsonProjection, ModError> {
        let kind = operation.string_at("/type").expect("validated type");
        let runtime_id = operation
            .string_at("/runtimeId")
            .expect("validated runtime id");
        let instance = {
            let mut data = lock(&self.client_ui.data);
            let Some(mut instance) = data.instances.get(runtime_id).cloned() else {
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            let parent_state = data.parents.get(&instance.key.site).map(|parent| {
                (
                    parent.revision,
                    parent.clients.contains_key(&instance.key.identity),
                    parent.clients.get(&instance.key.identity).cloned(),
                )
            });
            if kind != "unmount" {
                let Some((parent_revision, true, Some(parent_props))) = parent_state else {
                    return Ok(host_generated_json(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                };
                if parent_revision != revision {
                    return Ok(host_generated_json(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
                if !instance.mounted || instance.worker_fault.is_some() {
                    return Ok(host_generated_json(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
                instance.revision = revision;
                instance.props = parent_props;
                if let Some(stored) = data.instances.get_mut(runtime_id) {
                    stored.revision = revision;
                    stored.props = instance.props.clone();
                }
            } else {
                let same_identity_is_current =
                    parent_state.is_some_and(|(parent_revision, present, _)| {
                        present && (parent_revision != revision || instance.revision != revision)
                    });
                if same_identity_is_current {
                    return Ok(host_generated_json(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
            }
            instance
        };
        let plugin = host_plugin_name(instance.key.identity.plugin()).ok_or_else(|| {
            ModError::Hook("Client plugin identity is not valid host text".into())
        })?;
        if kind == "setProps" {
            let submitted = operation
                .subprojection("/props")
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            let data = lock(&self.client_ui.data);
            if data
                .parents
                .get(&instance.key.site)
                .and_then(|parent| parent.clients.get(&instance.key.identity))
                != Some(&submitted)
            {
                return Err(ModError::Hook(
                    "Client props must come from the latest validated parent tree".into(),
                ));
            }
        }
        let mut worker_operation = host_generated_json(json!({"type":kind,"runtimeId":runtime_id}));
        if let Some(generation) = instance.worker_generation {
            worker_operation
                .set_field(
                    "generation",
                    Utf16JsonProjection::parse(&generation.to_string())
                        .expect("generation is valid JSON"),
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        match kind {
            "setProps" => worker_operation
                .set_field(
                    "props",
                    operation
                        .subprojection("/props")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?,
            "resize" => {
                for field in ["columns", "rows"] {
                    worker_operation
                        .set_field(
                            field,
                            operation
                                .subprojection(&format!("/{field}"))
                                .map_err(|error| ModError::Protocol(error.to_string()))?,
                        )
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
            }
            "pointer" | "key" => worker_operation
                .set_field(
                    "payload",
                    operation
                        .subprojection("/event")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?,
            "runHeld" => {
                worker_operation
                    .set_field(
                        "handle",
                        operation
                            .subprojection("/handle")
                            .map_err(|error| ModError::Protocol(error.to_string()))?,
                    )
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                if operation.get_property("event").is_some() {
                    worker_operation
                        .set_field(
                            "payload",
                            operation
                                .subprojection("/event")
                                .map_err(|error| ModError::Protocol(error.to_string()))?,
                        )
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
            }
            "render" | "unmount" => {}
            _ => unreachable!("operation was validated"),
        }
        let mut post_action = if kind == "runHeld" {
            Some(self.client_ui.begin_post_action(runtime_id)?)
        } else {
            None
        };
        if let Some(post_action) = post_action.as_ref() {
            worker_operation
                .set_field(
                    "postActionId",
                    plain_text_projection(post_action.action_id()),
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        let mut pending_held_request = None;
        let result = match if let Some(post_action) = post_action.as_mut() {
            self.run_client_worker_operation_with_post_action(worker_operation, post_action)
                .await
                .map(|(result, _worker_epoch, request)| {
                    pending_held_request = request;
                    result
                })
        } else {
            self.run_client_worker_operation(worker_operation).await
        } {
            Ok(result) => result,
            Err(ClientWorkerOperationError::UnknownFault { phase, reason }) if kind == "runHeld" => {
                // Record the Host fence before the response can reach a client;
                // it blocks operations and background events for this VM.
                self.record_client_environment_fault(&instance, &phase, &reason);
                let mut response = worker_fault_response(
                    Some(runtime_id), revision, instance.key.identity.plugin(),
                    instance.key.identity.module(), &phase, &reason,
                );
                response.set_field("outcome", host_generated_json(json!({
                    "status":"unknown", "runtime":"unavailable",
                }))).map_err(|error| ModError::Protocol(error.to_string()))?;
                return Ok(response);
            }
            Err(ClientWorkerOperationError::Fault { phase, reason }) if kind != "unmount" => {
                self.record_client_worker_fault(runtime_id, &phase, &reason);
                return Ok(worker_fault_response(
                    Some(runtime_id),
                    revision,
                    instance.key.identity.plugin(),
                    instance.key.identity.module(),
                    &phase,
                    &reason,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let mut held_lease = pending_held_request.map(|mut request| {
            request.cancel_client_environment_id = instance.worker_environment_id.clone();
            ClientHeldLease { host: self, owner: instance.clone(), request, committed: false }
        });
        if let Some(post_action) = post_action.as_mut() {
            match client_run_held_result(&result)? {
                ClientRunHeldResult::Stale | ClientRunHeldResult::Rejected => {
                    return Ok(host_generated_json(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
                ClientRunHeldResult::Accepted(post_ids) => {
                    post_action.wait_for_posts(&post_ids).await?;
                }
            }
        }
        if kind == "unmount" {
            let handled = result
                .at_pointer("/unmounted")
                .and_then(|value| value.as_bool())
                == Some(true);
            let mut data = lock(&self.client_ui.data);
            self.client_ui
                .remove_instance_if_current(&mut data, runtime_id, &instance.key);
            return Ok(host_generated_json(
                json!({"handled":handled,"renderRevision":revision}),
            ));
        }
        if result
            .at_pointer("/stale")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            return Ok(host_generated_json(
                json!({"handled":false,"renderRevision":revision}),
            ));
        }
        if kind == "pointer" || kind == "key" || kind == "runHeld" {
            let mut render_operation = host_generated_json(json!({
                "type":"render", "runtimeId":runtime_id, "generation":instance.worker_generation,
            }));
            if let Some(action) = post_action.as_ref() {
                render_operation.set_field("postActionId", plain_text_projection(action.action_id()))
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            let rendered = match self
                .run_client_worker_operation(render_operation)
                .await
            {
                Ok(rendered) => rendered,
                Err(ClientWorkerOperationError::Fault { phase, reason }) => {
                    return Ok(worker_fault_response(
                        Some(runtime_id),
                        revision,
                        instance.key.identity.plugin(),
                        instance.key.identity.module(),
                        &phase,
                        &reason,
                    ));
                }
                Err(error) => return Err(error.into()),
            };
            if rendered
                .at_pointer("/stale")
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                if kind == "runHeld" {
                    // The callback already ran; a stale follow-up frame is
                    // not evidence that the action had no side effects.
                    return Err(ModError::Unavailable(
                        "Client runHeld was accepted but its follow-up frame is stale".into(),
                    ));
                }
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            }
            let frame = self
                .frame_for_worker_result(runtime_id, revision, &plugin, &rendered)
                .ok_or_else(|| {
                    ModError::Protocol("Client operation produced no render frame".into())
                })?;
            if kind == "runHeld" {
                let data = lock(&self.client_ui.data);
                if !self.client_instance_is_current(&data, &instance)
                    || post_action
                        .as_ref()
                        .is_some_and(|action| action.cancellation.is_cancelled())
                {
                    return Err(ModError::Unavailable(
                        "Client runHeld completed after its runtime changed or was cancelled"
                            .into(),
                    ));
                }
            }
            if kind == "runHeld" {
                let action = post_action.as_ref().expect("runHeld has a post action");
                let lease = held_lease.as_mut().ok_or_else(|| ModError::Protocol(
                    "accepted Client runHeld has no pending finalization lease".into(),
                ))?;
                lease.finalize(action.action_id()).await?;
                let data = lock(&self.client_ui.data);
                if !self.client_instance_is_current(&data, &instance) || action.cancellation.is_cancelled() {
                    return Err(ModError::Unavailable(
                        "Client runHeld finalized after its runtime changed or was cancelled".into(),
                    ));
                }
            }
            return Ok(frame);
        }
        if kind == "setProps" {
            self.update_instance_props(
                runtime_id,
                operation
                    .subprojection("/props")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            );
        }
        self.frame_for_worker_result(runtime_id, revision, &plugin, &result)
            .ok_or_else(|| ModError::Protocol("Client operation produced no render frame".into()))
    }

    async fn commit_client_draw(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_projection(operation)?;
        let client_count = operation.array_len("/clients").expect("validated clients");
        let mut identities = Vec::with_capacity(client_count);
        let mut seen = HashSet::new();
        for index in 0..client_count {
            let client_pointer = format!("/clients/{index}");
            let key = operation
                .string_units(&format!("{client_pointer}/key"))
                .expect("validated key");
            let identity = ClientUiIdentity::new(
                ExactHookText::from_json_projection(operation, &format!("{client_pointer}/plugin"))
                    .expect("plugin"),
                ExactHookText::from_json_projection(operation, &format!("{client_pointer}/module"))
                    .expect("module"),
                key,
            )
            .map_err(|error| ModError::Hook(error.to_string()))?;
            if !seen.insert(identity.clone()) {
                return Err(ModError::Hook(
                    "draw_commit contains a duplicate Client identity".into(),
                ));
            }
            identities.push(identity);
        }
        let identities_set = identities.iter().cloned().collect::<HashSet<_>>();
        let (handled, stale_runtimes, pending_event_runtimes) = {
            let mut data = lock(&self.client_ui.data);
            let Some(parent) = data
                .parents
                .get(&site)
                .filter(|parent| parent.revision == revision)
            else {
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            if identities
                .iter()
                .any(|identity| !parent.clients.contains_key(identity))
            {
                return Err(ModError::Hook(
                    "draw_commit contains a Client not present in the latest validated parent tree"
                        .into(),
                ));
            }
            let Some(token) = parent.token.as_ref() else {
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            let generation = token.generation();
            let handled = self
                .client_ui
                .fault_registry
                .finish_render(token, identities);
            if handled {
                if let Some(parent) = data.parents.get_mut(&site) {
                    parent.committed_generation = Some(generation);
                }
            }
            let stale_runtimes = data
                .instances
                .iter()
                .filter(|(_, instance)| {
                    instance.key.site == site && !identities_set.contains(&instance.key.identity)
                })
                .map(|(runtime_id, instance)| (runtime_id.clone(), instance.worker_generation))
                .collect::<Vec<_>>();
            for (runtime_id, _) in &stale_runtimes {
                if let Some(instance) = data.instances.remove(runtime_id) {
                    if data
                        .instance_by_identity
                        .get(&instance.key)
                        .map(String::as_str)
                        == Some(runtime_id)
                    {
                        data.instance_by_identity.remove(&instance.key);
                    }
                }
            }
            let pending_event_runtimes = if handled {
                data.instances
                    .iter_mut()
                    .filter_map(|(runtime_id, instance)| {
                        if instance.key.site == site
                            && instance.revision == revision
                            && identities_set.contains(&instance.key.identity)
                            && instance.mounted
                            && !instance.pending_events.is_empty()
                        {
                            instance.draining_pending_events = true;
                            Some(runtime_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            (handled, stale_runtimes, pending_event_runtimes)
        };
        for (runtime_id, generation) in stale_runtimes {
            self.client_ui.fail_post_actions_for_runtime(
                &runtime_id,
                "Client runtime was removed from the committed tree",
            );
            let mut unmount = json!({"type":"unmount","runtimeId":runtime_id});
            if let Some(generation) = generation {
                unmount["generation"] = json!(generation);
            }
            let _ = self
                .run_client_worker_operation(host_generated_json(unmount))
                .await;
        }
        for runtime_id in pending_event_runtimes {
            self.drain_client_ui_pending_events(&runtime_id, session)
                .await?;
        }
        Ok(host_generated_json(
            json!({"handled":handled,"renderRevision":revision}),
        ))
    }

    async fn unmount_client_site(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_projection(operation)?;
        let instances = {
            let mut data = lock(&self.client_ui.data);
            let Some(parent) = data
                .parents
                .get(&site)
                .filter(|parent| parent.revision == revision)
            else {
                return Ok(host_generated_json(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            if let Some(generation) = parent.committed_generation {
                self.client_ui
                    .fault_registry
                    .unmount_site(&site, generation);
            }
            let runtimes = data
                .instances
                .iter()
                .filter(|(_, instance)| instance.key.site == site)
                .map(|(runtime_id, _)| runtime_id.clone())
                .collect::<Vec<_>>();
            for runtime_id in &runtimes {
                if let Some(instance) = data.instances.remove(runtime_id) {
                    data.instance_by_identity.remove(&instance.key);
                }
            }
            data.parents.remove(&site);
            if data.latest_started.get(&site) == Some(&revision) {
                data.latest_started.remove(&site);
            }
            data.pending_tokens.remove(&(site.clone(), revision));
            let control_site = ParentControlSite {
                surface: "desktop".into(),
                component: site.component(),
                request_id: site.request_id().utf16_code_units.clone(),
            };
            if data.parent_control_latest.get(&control_site) == Some(&revision) {
                data.parent_control_latest.remove(&control_site);
                data.parent_control_renders.remove(&control_site);
            } else if data
                .parent_control_renders
                .get(&control_site)
                .is_some_and(|render| render.revision == revision)
            {
                data.parent_control_renders.remove(&control_site);
            }
            runtimes
        };
        self.remove_client_ui_render_state(
            "desktop",
            site.component().as_str(),
            &site.request_id().utf16_code_units,
        );
        for runtime_id in instances {
            self.client_ui
                .fail_post_actions_for_runtime(&runtime_id, "Client render site was unmounted");
            let _ = self
                .run_client_worker_operation(host_generated_json(
                    json!({"type":"unmount","runtimeId":runtime_id}),
                ))
                .await;
        }
        Ok(host_generated_json(
            json!({"handled":true,"renderRevision":revision}),
        ))
    }

    async fn drain_client_ui_pending_events(
        &self,
        runtime_id: &str,
        session: &dyn ModSessionContext,
    ) -> Result<(), ModError> {
        loop {
            let event = {
                let mut data = lock(&self.client_ui.data);
                let Some(instance) = data.instances.get_mut(runtime_id) else {
                    return Ok(());
                };
                if instance.pending_events.is_empty() {
                    instance.draining_pending_events = false;
                    return Ok(());
                }
                instance.pending_events.remove(0)
            };
            self.handle_client_ui_background_event_inner(&event, session, true, None)
                .await?;
        }
    }

    #[cfg(test)]
    pub(super) async fn handle_client_ui_background_event(
        &self,
        event: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<(), ModError> {
        self.handle_client_ui_background_event_inner(event, session, false, None)
            .await
    }

    pub(super) async fn handle_client_ui_background_event_in_epoch(
        &self,
        event: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
        worker_epoch: u64,
    ) -> Result<(), ModError> {
        self.handle_client_ui_background_event_inner(event, session, false, Some(worker_epoch))
            .await
    }

    async fn handle_client_ui_background_event_inner(
        &self,
        event: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
        from_pending_queue: bool,
        worker_epoch: Option<u64>,
    ) -> Result<(), ModError> {
        if event.string_at("/kind") != Some("ui.client.event") {
            return Ok(());
        }
        let mut post_ack = ClientPostEventGuard::new(
            Arc::clone(&self.client_ui.post_actions),
            event,
            worker_epoch,
        );
        let runtime_id = event
            .string_units("/runtimeId")
            .and_then(|units| String::from_utf16(&units).ok())
            .ok_or_else(|| ModError::Protocol("Client event has no runtime id".into()))?;
        let action = event.string_at("/action").unwrap_or("");
        let instance = {
            let mut data = lock(&self.client_ui.data);
            let Some(instance) = data.instances.get(&runtime_id).cloned() else {
                return Ok(());
            };
            if ExactHookText::from_json_projection(event, "/plugin").as_ref()
                != Some(instance.key.identity.plugin())
                || event.string_units("/element").as_deref()
                    != Some(instance.key.identity.key_units())
                || ExactHookText::from_json_projection(event, "/module").as_ref()
                    != Some(instance.key.identity.module())
            {
                return Ok(());
            }
            if let Some(generation) = instance.worker_generation {
                if event
                    .number_at("/generation")
                    .filter(|value| {
                        value.is_finite()
                            && value.fract() == 0.0
                            && *value >= 0.0
                            && *value <= MAX_JS_SAFE_INTEGER as f64
                    })
                    .map(|value| value as u64)
                    != Some(generation)
                {
                    return Ok(());
                }
            } else {
                if let Some(instance) = data.instances.get_mut(&runtime_id) {
                    let pending = client_pending_event(event, &mut post_ack, worker_epoch)?;
                    instance.pending_events.push(pending);
                }
                return Ok(());
            }
            if !data.parents.get(&instance.key.site).is_some_and(|parent| {
                parent.revision == instance.revision
                    && parent.clients.contains_key(&instance.key.identity)
            }) {
                return Ok(());
            }
            let drawn = self.client_ui.fault_registry.is_drawn(
                &crate::mod_ui_fault::ClientUiFaultAddress::new(
                    instance.key.site.clone(),
                    instance.key.identity.clone(),
                ),
            );
            if from_pending_queue && !drawn {
                return Ok(());
            }
            if !drawn || (!from_pending_queue && instance.draining_pending_events) {
                if let Some(instance) = data.instances.get_mut(&runtime_id) {
                    let pending = client_pending_event(event, &mut post_ack, worker_epoch)?;
                    instance.pending_events.push(pending);
                }
                return Ok(());
            }
            instance
        };
        let plugin = host_plugin_name(instance.key.identity.plugin()).ok_or_else(|| {
            ModError::Hook("Client plugin identity is not valid host text".into())
        })?;
        if instance.worker_fault.is_some() && action != "fault" {
            return Ok(());
        }
        match action {
            "schedule" => {
                let result = self
                    .run_client_worker_operation(host_generated_json(json!({
                        "type":"render","runtimeId":runtime_id,
                        "generation":instance.worker_generation,
                    })))
                    .await?;
                if let Some(frame) =
                    self.frame_for_worker_result(&runtime_id, instance.revision, &plugin, &result)
                {
                    if self
                        .current_instance(&instance.key.site, &instance.key.identity)
                        .is_some()
                    {
                        let _ = self
                            .emit_client_frame(session, &runtime_id, instance.revision, &frame)
                            .await;
                    }
                }
            }
            "post" => {
                let json_units = event.string_units("/json").ok_or_else(|| {
                    ModError::Protocol("Client post event has invalid JSON".into())
                })?;
                let data = Utf16JsonProjection::parse_utf16_json(&json_units)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let request = Utf16JsonProjection::object(vec![
                    ("subtype".into(), plain_text_projection("ui_message")),
                    (
                        "plugin".into(),
                        exact_text_projection(instance.key.identity.plugin()),
                    ),
                    (
                        "component".into(),
                        plain_text_projection(instance.key.site.component().as_str()),
                    ),
                    (
                        "instance_id".into(),
                        exact_text_projection(instance.key.site.request_id()),
                    ),
                    (
                        "client".into(),
                        exact_string_projection(instance.key.identity.key_units().to_vec()),
                    ),
                    (
                        "module".into(),
                        exact_text_projection(instance.key.identity.module()),
                    ),
                    ("data".into(), data),
                ])
                .map_err(|error| ModError::Protocol(error.to_string()))?;
                let response = if let Some(post_ack) = post_ack.as_ref() {
                    let cancellation = post_ack.cancellation.clone();
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Ok(()),
                        response = self.dispatch_client_message(&request, session, post_ack.action_id.as_deref()) => response,
                    }
                } else {
                    self.dispatch_client_message(&request, session, None).await
                };
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        if let Some(post_ack) = post_ack.take() {
                            post_ack.fail(&format!("Client message outcome is unknown: {error}"));
                        }
                        return Err(error);
                    }
                };
                if post_ack
                    .as_ref()
                    .is_some_and(|post_ack| post_ack.cancellation.is_cancelled())
                {
                    return Ok(());
                }
                if response
                    .at_pointer("/handled")
                    .and_then(|value| value.as_bool())
                    != Some(false)
                {
                    if let Some(post_ack) = post_ack.take() {
                        // Keep the instance check and acknowledgement under
                        // the same lock as Host removal. Removal can otherwise
                        // win between the check and the Settled notification.
                        let data = lock(&self.client_ui.data);
                        if self.client_instance_is_current(&data, &instance) {
                            post_ack.complete();
                        } else {
                            post_ack.fail("Client runtime changed before the post completed");
                        }
                    }
                }
            }
            "fault" => {
                let event_phase = event
                    .string_at("/phase")
                    .filter(|phase| matches!(*phase, "load" | "render" | "run"))
                    .unwrap_or("run");
                let event_reason = ExactHookText::from_json_projection(event, "/reason")
                    .unwrap_or_else(|| ExactHookText::from_text("Client surface failed"));
                self.record_client_worker_fault(&runtime_id, event_phase, &event_reason);
                let Some(fault) = self.client_worker_fault(&runtime_id) else {
                    return Ok(());
                };
                let phase = fault.phase.as_str();
                let reason = &fault.reason;
                if let Some(claim) = self.claim_client_worker_fault_snapshot(&runtime_id) {
                    let normalized_reason = crate::mod_ui_fault::normalize_client_fault_reason(
                        instance.key.identity.plugin(),
                        instance.key.identity.module(),
                        reason,
                    );
                    let fault = Utf16JsonProjection::object(vec![
                        ("phase".into(), plain_text_projection(phase)),
                        ("reason".into(), exact_text_projection(&normalized_reason)),
                        ("source".into(), plain_text_projection("worker")),
                    ])
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                    let failure = Utf16JsonProjection::object(vec![
                        (
                            "renderRevision".into(),
                            Utf16JsonProjection::parse(&instance.revision.to_string())
                                .expect("revision is a valid JSON number"),
                        ),
                        ("fault".into(), fault),
                    ])
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                    // This is a Host-local leaf failure snapshot for async
                    // worker faults, not a successful tree frame or a control response.
                    match session.emit_mod_ui_client_frame(&runtime_id, &failure).await {
                        Ok(()) => claim.commit(),
                        Err(error) => tracing::warn!(%error, "client fault frame could not be admitted; snapshot remains pending"),
                    }
                }
                if !self.claim_client_worker_fault_dispatch(&runtime_id) {
                    return Ok(());
                }
                let request = Utf16JsonProjection::object(vec![
                    ("subtype".into(), plain_text_projection("ui_client_fault")),
                    (
                        "plugin".into(),
                        exact_text_projection(instance.key.identity.plugin()),
                    ),
                    (
                        "component".into(),
                        plain_text_projection(instance.key.site.component().as_str()),
                    ),
                    (
                        "instance_id".into(),
                        exact_text_projection(instance.key.site.request_id()),
                    ),
                    (
                        "client".into(),
                        exact_string_projection(instance.key.identity.key_units().to_vec()),
                    ),
                    (
                        "module".into(),
                        exact_text_projection(instance.key.identity.module()),
                    ),
                    ("phase".into(), plain_text_projection(phase)),
                    ("reason".into(), exact_text_projection(reason)),
                ])
                .map_err(|error| ModError::Protocol(error.to_string()))?;
                let _ = self.dispatch_client_fault(&request, session).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

struct ClientFaultHostAdapter<'a> {
    host: &'a ModHost,
    session: &'a dyn ModSessionContext,
    session_id: String,
}

#[async_trait::async_trait]
impl ClientUiFaultHost for ClientFaultHostAdapter<'_> {
    type Error = ModError;

    async fn dispatch_plugin_ui_fault(
        &self,
        plugin: &ExactHookText,
        event: &UiFaultHookEvent,
    ) -> Result<(), Self::Error> {
        let Some(plugin) = host_plugin_name(plugin) else {
            return Ok(());
        };
        self.host
            .dispatch_plugin_scoped_ui_event(
                "ui.fault",
                &plugin,
                event.to_projection(),
                self.session,
                None,
                None,
                |_| async { Ok(Some(host_generated_json(json!({"handled":true})))) },
            )
            .await
            .map(|_| ())
    }

    async fn has_matching_ui_fault_listener(
        &self,
        plugin: &ExactHookText,
        event: &UiFaultHookEvent,
    ) -> bool {
        let Some(plugin) = host_plugin_name(plugin) else {
            return false;
        };
        self.host
            .has_matching_ui_listener(&plugin, "ui.fault", &event.to_projection())
            .await
            // Native treats a matcher exception as a match; fail open only
            // for the invalidation calculation after the hook has run.
            .unwrap_or(true)
    }

    fn current_ui_render_version(&self, site: &ClientUiRenderSite) -> u64 {
        self.host.current_client_ui_render_state_version(
            "desktop",
            site.component().as_str(),
            &site.request_id().utf16_code_units,
        )
    }

    async fn invalidate_ui_render_site(&self, site: &ClientUiRenderSite) {
        let sequence = self
            .host
            .client_ui
            .next_invalidation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let uuid = format!("mod-ui-{}-{sequence}", self.host.instance_id);
        let instance = Utf16JsonProjection::object(vec![
            ("surface".into(), plain_text_projection("desktop")),
            (
                "component".into(),
                plain_text_projection(site.component().as_str()),
            ),
            (
                "instance_id".into(),
                exact_text_projection(site.request_id()),
            ),
        ])
        .expect("invalidation instance fields form an object");
        let instances = Utf16JsonProjection::array(vec![instance])
            .expect("invalidation instance forms an array");
        self.session
            .emit_mod_ui_invalidate(Some(&instances), &uuid, &self.session_id)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_value(projection: &Utf16JsonProjection) -> Value {
        let source = projection
            .to_json_string()
            .expect("test projection is valid JSON");
        serde_json::from_str(&source).expect("test projection serializes as JSON")
    }

    #[test]
    fn run_held_reply_requires_an_unambiguous_acceptance() {
        let reply = |value| host_generated_json(value);
        assert!(matches!(
            client_run_held_result(&reply(json!({"stale":true}))),
            Ok(ClientRunHeldResult::Stale)
        ));
        assert!(matches!(
            client_run_held_result(&reply(json!({"accepted":false,"postIds":[]}))),
            Ok(ClientRunHeldResult::Rejected)
        ));
        assert!(matches!(
            client_run_held_result(&reply(json!({"accepted":true,"postIds":[]}))),
            Ok(ClientRunHeldResult::Accepted(post_ids)) if post_ids.is_empty()
        ));
        match client_run_held_result(&reply(json!({"accepted":true,"postIds":[0,2]}))).unwrap() {
            ClientRunHeldResult::Accepted(post_ids) => assert_eq!(post_ids, vec![0, 2]),
            _ => panic!("accepted callback must preserve its synchronous post IDs"),
        }
        for invalid in [
            json!({}),
            json!({"accepted":"true","postIds":[]}),
            json!({"accepted":true}),
            json!({"accepted":false,"postIds":[0]}),
            json!({"stale":false}),
            json!({"stale":true,"accepted":true,"postIds":[]}),
            json!({"stale":true,"fault":{"phase":"run"}}),
            json!({"accepted":false,"postIds":[],"fault":{"phase":"run"}}),
            json!({"accepted":true,"postIds":[],"fault":{"phase":"run"}}),
            json!({"stale":true,"unexpected":true}),
            json!({"accepted":false,"postIds":[],"unexpected":true}),
            json!({"accepted":true,"postIds":[],"unexpected":true}),
            json!([]),
        ] {
            assert!(
                client_run_held_result(&reply(invalid.clone())).is_err(),
                "malformed or contradictory reply must remain an error: {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn client_post_action_keeps_early_acks_and_releases_on_worker_epoch_end() {
        let registry = Arc::new(ClientPostAckRegistry::default());
        let mut early = registry
            .register("early-action".into(), "runtime-a".into())
            .unwrap();
        early.bind_worker_epoch(9).unwrap();
        for post_id in [2, 3] {
            registry.notify(
                "early-action",
                "runtime-a",
                9,
                ClientPostAck::Settled {
                    worker_epoch: 9,
                    post_id,
                },
            );
        }
        early.wait_for_posts(&[2, 3]).await.unwrap();
        let early_cancellation = registry.cancellation_for("early-action", "runtime-a", Some(9));
        drop(early);
        assert!(early_cancellation.is_cancelled());

        let mut awaiting_no_props = registry
            .register("no-props-action".into(), "runtime-a".into())
            .unwrap();
        awaiting_no_props.bind_worker_epoch(9).unwrap();
        for post_id in [0, 1] {
            registry.notify(
                "no-props-action",
                "runtime-a",
                9,
                ClientPostAck::Settled {
                    worker_epoch: 9,
                    post_id,
                },
            );
        }
        let waiter =
            tokio::spawn(async move { awaiting_no_props.wait_for_posts(&[0, 1, 2]).await });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(
            !waiter.is_finished(),
            "posts without props still have to settle before the frame can return"
        );
        registry.notify(
            "no-props-action",
            "runtime-a",
            9,
            ClientPostAck::Settled {
                worker_epoch: 9,
                post_id: 2,
            },
        );
        waiter.await.unwrap().unwrap();

        let mut cancelled_after_ack = registry
            .register("cancelled-after-ack".into(), "runtime-b".into())
            .unwrap();
        cancelled_after_ack.bind_worker_epoch(9).unwrap();
        registry.notify(
            "cancelled-after-ack",
            "runtime-b",
            9,
            ClientPostAck::Settled {
                worker_epoch: 9,
                post_id: 0,
            },
        );
        registry.fail_runtime("runtime-b", "runtime removed");
        assert!(cancelled_after_ack.wait_for_posts(&[0]).await.is_err());

        let mut cancelled_without_posts = registry
            .register("cancelled-without-posts".into(), "runtime-c".into())
            .unwrap();
        cancelled_without_posts.bind_worker_epoch(9).unwrap();
        registry.fail_runtime("runtime-c", "runtime removed");
        assert!(cancelled_without_posts.wait_for_posts(&[]).await.is_err());

        let mut interrupted = registry
            .register("interrupted-action".into(), "runtime-b".into())
            .unwrap();
        interrupted.bind_worker_epoch(10).unwrap();
        registry.fail_epoch(10, "worker exited");
        let error = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            interrupted.wait_for_posts(&[0]),
        )
        .await
        .expect("worker exit releases the action without waiting for the settlement deadline")
        .unwrap_err();
        assert!(error.to_string().contains("worker exited"));
    }

    fn parent_action_handle(value: Option<&Value>) -> Option<u64> {
        let handle = value?.as_u64()?;
        (handle > 0 && handle <= MAX_JS_SAFE_INTEGER).then_some(handle)
    }

    #[derive(Default)]
    struct UiLifecycleSession {
        root: std::path::PathBuf,
        frames: StdMutex<Vec<(String, Value)>>,
        raw_frames: StdMutex<Vec<(String, Utf16JsonProjection)>>,
        frame_gate: StdMutex<Option<UiLifecycleFrameGate>>,
        model_gate: StdMutex<Option<UiLifecycleModelGate>>,
        logs: StdMutex<Vec<String>>,
        allow_non_scalar_frames: bool,
        invalidations: StdMutex<Vec<Value>>,
    }

    struct UiLifecycleFrameGate {
        runtime_id: String,
        expected_text: &'static str,
        entered: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
        delivered: std::sync::Arc<tokio::sync::Notify>,
    }

    struct UiLifecycleModelGate {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for UiLifecycleSession {
        fn cwd(&self) -> std::path::PathBuf {
            self.root.clone()
        }

        fn root(&self) -> std::path::PathBuf {
            self.root.clone()
        }

        async fn model(&self) -> String {
            let gate = lock(&self.model_gate).take();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
                gate.completed.notify_one();
            }
            "test-model".into()
        }

        async fn id(&self) -> String {
            "ui-lifecycle-session".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn emit_mod_log(&self, _plugin: &str, text: Utf16JsonProjection) {
            if let Some(text) = text.string_at("") {
                lock(&self.logs).push(text.to_owned());
            }
        }

        async fn emit_mod_ui_client_frame(
            &self,
            runtime_id: &str,
            frame: &Utf16JsonProjection,
        ) -> Result<(), ModError> {
            let gate = {
                let mut gate = lock(&self.frame_gate);
                if gate.as_ref().is_some_and(|gate| {
                    gate.runtime_id == runtime_id
                        && projection_tree_contains_text(frame, gate.expected_text)
                }) {
                    gate.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate.as_ref() {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            // Preserve raw frames for assertions that include lone UTF-16
            // units. Ordinary tests may still use Value as a semantic oracle.
            lock(&self.raw_frames).push((runtime_id.to_owned(), frame.clone()));
            let source = frame.to_json_string().expect("valid test frame projection");
            match serde_json::from_str(&source) {
                Ok(frame) => lock(&self.frames).push((runtime_id.to_owned(), frame)),
                Err(error) => assert!(
                    self.allow_non_scalar_frames,
                    "test frame must be a serde_json Value: {error}"
                ),
            }
            if let Some(gate) = gate {
                gate.delivered.notify_one();
            }
            Ok(())
        }

        async fn emit_mod_ui_invalidate(
            &self,
            instances: Option<&Utf16JsonProjection>,
            uuid: &str,
            session_id: &str,
        ) {
            let instances = instances
                .map(|projection| {
                    let source = projection
                        .to_json_string()
                        .expect("valid test invalidation projection");
                    serde_json::from_str(&source).expect("bounded valid test invalidation")
                })
                .unwrap_or(Value::Null);
            lock(&self.invalidations).push(json!({
                "instances":instances,
                "uuid":uuid,
                "session_id":session_id,
            }));
        }
    }

    fn tree_contains_text(tree: &Value, expected: &str) -> bool {
        if tree.as_str() == Some(expected) {
            return true;
        }
        tree.get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| {
                children
                    .iter()
                    .any(|child| tree_contains_text(child, expected))
            })
    }

    fn tree_node_with_key<'a>(tree: &'a Value, key: &str) -> Option<&'a Value> {
        if tree.pointer("/props/key").and_then(Value::as_str) == Some(key) {
            return Some(tree);
        }
        tree.get("children")
            .and_then(Value::as_array)
            .and_then(|children| {
                children
                    .iter()
                    .find_map(|child| tree_node_with_key(child, key))
            })
    }

    fn tree_press_handle_for_key(tree: &Value, key: &str) -> Option<u64> {
        tree_node_with_key(tree, key)
            .and_then(|node| node.pointer("/press/handle"))
            .and_then(Value::as_u64)
    }

    fn tree_first_press_handle(tree: &Value) -> Option<u64> {
        tree.pointer("/press/handle")
            .and_then(Value::as_u64)
            .or_else(|| {
                tree.get("children")
                    .and_then(Value::as_array)
                    .and_then(|children| children.iter().find_map(tree_first_press_handle))
            })
    }

    fn projection_tree_contains_text(projection: &Utf16JsonProjection, expected: &str) -> bool {
        use lingxi_core::types::utf16_json::JsonValueView;

        fn contains_text(tree: JsonValueView<'_>, expected: &str) -> bool {
            if tree.as_str() == Some(expected) {
                return true;
            }
            if let Some(length) = tree.array_len() {
                return (0..length).any(|index| {
                    tree.get_index(index)
                        .is_some_and(|child| contains_text(child, expected))
                });
            }
            tree.object_entries().is_some_and(|entries| {
                entries
                    .into_iter()
                    .any(|entry| contains_text(entry.value, expected))
            })
        }

        projection
            .get_property("tree")
            .is_some_and(|tree| contains_text(tree, expected))
    }

    async fn wait_for_exact_client_frame(
        session: &UiLifecycleSession,
        runtime_id: &str,
        stage: &str,
        predicate: impl Fn(&Utf16JsonProjection) -> bool,
    ) -> Utf16JsonProjection {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frame = {
                    let frames = lock(&session.raw_frames);
                    frames
                        .iter()
                        .rev()
                        .find(|(seen_runtime, frame)| {
                            seen_runtime == runtime_id && predicate(frame)
                        })
                        .map(|(_, frame)| frame.clone())
                };
                if let Some(frame) = frame {
                    return frame;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let captured = lock(&session.raw_frames)
                .iter()
                .filter(|(seen_runtime, _)| seen_runtime == runtime_id)
                .map(|(_, frame)| frame.clone())
                .collect::<Vec<_>>();
            panic!("Client UI frame did not arrive during {stage}; captured frames: {captured:?}")
        })
    }

    async fn wait_for_client_frame(
        session: &UiLifecycleSession,
        runtime_id: &str,
        stage: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frame = {
                    let frames = lock(&session.frames);
                    frames
                        .iter()
                        .rev()
                        .find(|(seen_runtime, frame)| {
                            seen_runtime == runtime_id && predicate(frame)
                        })
                        .map(|(_, frame)| frame.clone())
                };
                if let Some(frame) = frame {
                    return frame;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let captured = lock(&session.frames)
                .iter()
                .filter(|(seen_runtime, _)| seen_runtime == runtime_id)
                .map(|(_, frame)| frame.clone())
                .collect::<Vec<_>>();
            panic!("Client UI frame did not arrive during {stage}; captured frames: {captured:?}")
        })
    }

    #[tokio::test]
    async fn client_state_token_tracks_only_committed_native_render_dependencies() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(
            &child,
            r#"
              import { Box, Text, h } from 'claude:surface-runtime';
              export function Card(props: { label: string }) {
                return h(Box, { children: [h(Text, { children: props.label })] });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, async ($, event) => {
                  const current = await $.state.get({ plugin: 'stateful', key: 'screen' });
                  const { Client, h } = $.ui.resolve(event);
                  return h(Client, {
                    key: 'card', module: './card.tsx',
                    props: { label: current.value?.label ?? 'initial' },
                  });
                });
                on('ui.render', { surface: 'terminal', component: 'AbovePrompt' }, async ($, event) => {
                  const current = await $.state.get({ plugin: 'stateful', key: 'screen' });
                  const { Text } = $.ui.resolve(event);
                  return Text({ children: [current.value?.label ?? 'initial'] });
                });
                on('turn.start', async ($, event, next) => {
                  await $.state.set({ plugin: 'stateful', key: 'screen' }, { label: 'updated' });
                  return next(event);
                });
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "stateful",
            "stateful@user",
            directory.path(),
            &entry,
            Utf16JsonProjection::parse("{}").unwrap(),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let render = || {
            json!({
                "subtype":"ui_render",
                "surface":"desktop",
                "component":"Pane",
                "instance_id":"pane-state",
                "props":{},
                "viewport":{"columns":80,"rows":24},
            })
        };
        let initial = host
            .dispatch_client_ui_control(host_generated_json(render()), session.as_ref())
            .await
            .unwrap();
        let initial_revision = initial
            .render_revision
            .expect("successful ui.render carries a host revision");
        let initial_site = client_site_from_input(&render()).unwrap();
        assert_eq!(
            test_value(&initial.response)["tree"]["type"],
            "Client",
            "{:#?}",
            initial.response
        );
        assert!(
            test_value(&initial.response)["client_modules"]["stateful"].is_string(),
            "loaded Client manifest is missing: {:#?}",
            initial.response
        );
        {
            let data = lock(&host.client_ui.data);
            assert_eq!(
                data.latest_started.get(&initial_site),
                Some(&initial_revision)
            );
            assert!(data.parents.contains_key(&initial_site));
        }
        let initial_token = initial.client_state_token.as_deref().unwrap_or_else(|| {
            panic!(
                "validated Client tree carries its state token; response={:#?}",
                initial.response
            )
        });
        assert!(initial_token.bytes().all(|byte| byte.is_ascii_digit()));
        let module = test_value(&initial.response)["tree"]["props"]["module"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(module, "card.tsx");
        assert_eq!(
            test_value(&initial.response)["tree"]["props"]["props"]["label"],
            "initial"
        );
        let committed = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":initial_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);

        let unchanged = host
            .dispatch_client_ui_control(host_generated_json(render()), session.as_ref())
            .await
            .unwrap();
        assert_eq!(unchanged.client_state_token.as_deref(), Some(initial_token));
        let unchanged_revision = unchanged.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":unchanged_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);

        let mut wider_viewport_render = render();
        wider_viewport_render["viewport"]["columns"] = json!(120);
        let wider_viewport = host
            .dispatch_client_ui_control(
                host_generated_json(wider_viewport_render),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            wider_viewport.client_state_token.as_deref(),
            Some(initial_token),
            "viewport is a separate invalidation input and is excluded from Rh's render signature"
        );
        let wider_viewport_revision = wider_viewport.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":wider_viewport_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);

        let fault = host
            .dispatch_client_ui_control(
                host_generated_json(json!({
                    "subtype":"ui_client_fault",
                    "plugin":"stateful",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "client":"card",
                    "module":"card.tsx",
                    "phase":"run",
                    "reason":"test fault",
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&fault.response)["handled"], true);
        assert_eq!(fault.client_state_token, None);

        let after_fault = host
            .dispatch_client_ui_control(host_generated_json(render()), session.as_ref())
            .await
            .unwrap();
        assert_eq!(
            after_fault.client_state_token.as_deref(),
            Some(initial_token)
        );
        let after_fault_revision = after_fault.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":after_fault_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);

        let above_prompt = host
            .dispatch_ui_render_with_state_scope_at_session(
                host_generated_json(json!({
                    "surface":"terminal",
                    "component":"AbovePrompt",
                    "requestId":"above-prompt-state",
                    "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,
                        "bodyColumns":80,"scroll":{"offset":0,"bodyRows":2},"view":{}},
                    "viewport":{"columns":80,"rows":24,"isFullscreen":false},
                })),
                1,
                session.as_ref(),
                |event| async move { Ok(event) },
                |_: String, _: Utf16JsonProjection| async {},
                |_: String, _: Utf16JsonProjection, _: u64| async {},
                |_: String, _: Option<Utf16JsonProjection>| async {},
            )
            .await
            .unwrap();
        assert_eq!(
            above_prompt.result_projection.string_at("/children/0"),
            Some("initial")
        );

        // Seed the two Native pace folds as pending with a fake monotonic
        // clock. The following `state.set` still traverses the real Worker/API
        // path and must flush both pending folds before that API reply.
        let state_key = crate::mods::ModStateKey::test("stateful", "screen");
        let now_ms = host.ui_pacing_now_ms();
        {
            let mut state = lock(&host.state);
            state.ui_render_state_folds.0.last_at_ms = Some(now_ms.saturating_add(1_000));
            state.ui_render_state_folds.1.last_at_ms = Some(now_ms.saturating_add(1_000));
            let pending = state.schedule_ui_render_write(&state_key, now_ms);
            assert!(pending.sites.is_empty());
            assert_eq!(pending.timers.len(), 2);
            assert_eq!(
                pending
                    .timers
                    .iter()
                    .map(|timer| timer.deadline_ms - now_ms)
                    .collect::<HashSet<_>>(),
                HashSet::from([34, 100])
            );
        }
        let invalidation_start = lock(&session_impl.invalidations).len();
        host.dispatch_with_log_at_session(
            "turn.start",
            host_generated_json(json!({"turnId":"state-change"})),
            session.as_ref(),
            |event| async move { Ok(event) },
            |_, _| async {},
        )
        .await
        .unwrap();
        let invalidations = lock(&session_impl.invalidations);
        let state_invalidation = invalidations[invalidation_start..]
            .iter()
            .find(|entry| {
                entry["instances"].as_array().is_some_and(|instances| {
                    instances.iter().any(|instance| {
                        instance["component"] == "Pane" && instance["instance_id"] == "pane-state"
                    }) && instances.iter().any(|instance| {
                        instance["component"] == "AbovePrompt"
                            && instance["instance_id"] == "above-prompt-state"
                    })
                })
            })
            .expect("state.set flushes both pending pace folds before replying");
        let pane = state_invalidation["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|instance| instance["component"] == "Pane")
            .unwrap();
        assert_eq!(pane["surface"], "desktop");
        drop(invalidations);

        let updated = host
            .dispatch_client_ui_control(host_generated_json(render()), session.as_ref())
            .await
            .unwrap();
        let updated_token = updated.client_state_token.as_deref().unwrap();
        assert_ne!(updated_token, initial_token);
        assert_eq!(
            test_value(&updated.response)["tree"]["props"]["props"]["label"],
            "updated"
        );
        let updated_revision = updated.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":updated_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);
    }

    #[tokio::test]
    async fn unmatched_plugin_set_change_advances_each_failed_client_site_token() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let owner_entry = directory.path().join("owner.tsx");
        let unrelated_entry = directory.path().join("unrelated.js");
        std::fs::write(
            &child,
            r#"
              import { Box, Text, h } from 'claude:surface-runtime';
              export function Card(props: { label: string }) {
                return h(Box, { children: [h(Text, { children: props.label })] });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &owner_entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, async ($, event) => {
                  const { Client, h } = $.ui.resolve(event);
                  return h(Client, {
                    key: 'card', module: './card.tsx', props: { label: event.requestId },
                  });
                });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &unrelated_entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'AbovePrompt' }, async () => null);
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));
        let prepared = host
            .prepare_module(directory.path(), &owner_entry)
            .await
            .unwrap();
        host.load_with_tier_order_storage_prepared(
            "owner",
            "owner@user",
            directory.path(),
            &owner_entry,
            Utf16JsonProjection::parse("{}").unwrap(),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        async fn render_and_commit(
            host: &ModHost,
            session: &dyn ModSessionContext,
            request_id: &str,
        ) -> String {
            let response = host
                .dispatch_client_ui_control(
                    host_generated_json(json!({
                        "subtype":"ui_render",
                        "surface":"desktop",
                        "component":"Pane",
                        "instance_id":request_id,
                        "props":{},
                        "viewport":{"columns":80,"rows":24},
                    })),
                    session,
                )
                .await
                .unwrap();
            assert_eq!(test_value(&response.response)["tree"]["type"], "Client");
            let token = response
                .client_state_token
                .clone()
                .expect("validated Client tree carries a state token");
            let revision = response.render_revision.unwrap();
            let committed = host
                .dispatch_client_ui_operation(
                    host_generated_json(json!({
                        "type":"draw_commit",
                        "surface":"desktop",
                        "component":"Pane",
                        "instance_id":request_id,
                        "render_revision":revision,
                        "clients":[{"plugin":"owner","key":"card","module":"card.tsx"}],
                    })),
                    session,
                )
                .await
                .unwrap();
            assert_eq!(test_value(&committed)["handled"], true);
            token
        }

        let first_site = "pane-global-a";
        let second_site = "pane-global-b";
        let first_token = render_and_commit(&host, session.as_ref(), first_site).await;
        let second_token = render_and_commit(&host, session.as_ref(), second_site).await;
        assert_eq!(
            render_and_commit(&host, session.as_ref(), first_site).await,
            first_token,
            "an unchanged render must preserve its token"
        );
        assert_eq!(
            render_and_commit(&host, session.as_ref(), second_site).await,
            second_token,
            "a second site's unchanged render must preserve its token"
        );

        let fault = host
            .dispatch_client_ui_control(
                host_generated_json(json!({
                    "subtype":"ui_client_fault",
                    "plugin":"owner",
                    "component":"Pane",
                    "instance_id":first_site,
                    "client":"card",
                    "module":"card.tsx",
                    "phase":"run",
                    "reason":"test fault",
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&fault.response)["handled"], true);
        assert_eq!(
            render_and_commit(&host, session.as_ref(), first_site).await,
            first_token,
            "same-token rerender must keep the existing Client fault identity"
        );
        assert_eq!(
            render_and_commit(&host, session.as_ref(), second_site).await,
            second_token,
            "a fault at one site must not change another site's token"
        );

        let invalidation_start = lock(&session_impl.invalidations).len();
        host.load(
            "unrelated",
            directory.path(),
            &unrelated_entry,
            Utf16JsonProjection::parse("{}").unwrap(),
        )
        .await
        .unwrap();
        let lifecycle_invalidations = lock(&session_impl.invalidations)
            .iter()
            .skip(invalidation_start)
            .cloned()
            .collect::<Vec<_>>();
        let invalidated_sites = lifecycle_invalidations
            .iter()
            .flat_map(|entry| entry["instances"].as_array().into_iter().flatten())
            .map(|site| site["instance_id"].as_str().unwrap_or_default().to_owned())
            .collect::<HashSet<_>>();
        assert!(invalidated_sites.contains(first_site));
        assert!(invalidated_sites.contains(second_site));

        let first_after_global = render_and_commit(&host, session.as_ref(), first_site).await;
        let second_after_global = render_and_commit(&host, session.as_ref(), second_site).await;
        assert_ne!(first_after_global, first_token);
        assert_ne!(second_after_global, second_token);
        assert_eq!(
            render_and_commit(&host, session.as_ref(), first_site).await,
            first_after_global,
            "the new global generation remains stable without another change"
        );
    }

    #[test]
    fn parent_held_control_normalization_defaults_surface_and_strips_unknowns() {
        let normalized = normalize_control_request(&host_generated_json(json!({
            "subtype":"ui_input",
            "plugin":"review",
            "handle":7,
            "kind":"change",
            "value":"hello",
            "key":"search",
            "component":"Pane",
            "instance_id":"pane-1",
            "client_id":"desktop-1",
            "unrecognized":"discarded"
        })))
        .unwrap();

        assert_eq!(
            test_value(&normalized),
            json!({
                "subtype":"ui_input",
                "plugin":"review",
                "handle":7,
                "kind":"change",
                "value":"hello",
                "key":"search",
                "component":"Pane",
                "instance_id":"pane-1",
                "client_id":"desktop-1",
                "surface":"desktop"
            })
        );
    }

    #[test]
    fn parent_control_normalization_strips_nested_unknowns_but_keeps_user_props() {
        let normalized = normalize_control_request(&host_generated_json(json!({
            "subtype":"ui_render",
            "surface":"desktop",
            "component":"Pane",
            "instance_id":"pane-1",
            "props":{"custom":{"kept":true}},
            "viewport":{"columns":80,"rows":24,"ignored":true},
            "bench":{"seq":2,"t0":3.5,"ignored":true},
            "ignored":true
        })))
        .unwrap();

        assert_eq!(
            test_value(&normalized),
            json!({
                "subtype":"ui_render",
                "surface":"desktop",
                "component":"Pane",
                "instance_id":"pane-1",
                "props":{"custom":{"kept":true}},
                "viewport":{"columns":80,"rows":24},
                "bench":{"seq":2,"t0":3.5}
            })
        );
    }

    #[test]
    fn ui_message_projection_preserves_lone_surrogate_client_and_nested_data() {
        let raw = r#"{"subtype":"ui_message","plugin":"review","component":"Pane","instance_id":"pane-1","client":"\ud800","module":"surface.js","data":{"\ud800":"\udfff"}}"#;
        let parsed = Utf16JsonProjection::parse(raw).unwrap();
        let normalized = normalize_control_request(&parsed).unwrap();
        validate_control_request(&normalized).unwrap();

        assert_eq!(normalized.string_units("/client"), Some(vec![0xd800]));
        let data = normalized.subprojection("/data").unwrap();
        let key = data.object_entries().unwrap()[0].code_units.to_vec();
        assert_eq!(key, vec![0xd800]);
        let value = data.subprojection_property_units(&key).unwrap();
        assert_eq!(value.string_units(""), Some(vec![0xdfff]));
        let identity = client_identity_from_control(&normalized).unwrap();
        assert_eq!(identity.key_units(), &[0xd800]);
        assert!(normalized
            .to_json_string()
            .unwrap()
            .contains(r#""client":"\ud800""#));
    }

    #[tokio::test]
    async fn native_client_event_keeps_exact_identity_and_nested_post_data() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(
            &child,
            r#"
              import { Text, h } from 'claude:surface-runtime';
              export function Card(props: { valid: boolean }, surface: any) {
                if (!surface.state.sent) {
                  surface.setState({ sent: true });
                  surface.post(JSON.parse('{"\\ud800":"\\udfff"}'));
                }
                return h(Text, { children: props.valid ? 'exact-utf16' : 'waiting' });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
                  const { Client, h } = $.ui.resolve(event);
                  return h(Client, {
                    key: '\ud800', module: './card.tsx', props: { valid: false },
                  });
                });
                on('ui.message', (_$, event) => {
                  const key = Object.keys(event.data)[0];
                  return { props: { valid: key === '\ud800' && event.data[key] === '\udfff' } };
                });
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            allow_non_scalar_frames: true,
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "utf16",
            "utf16@user",
            directory.path(),
            &entry,
            Utf16JsonProjection::parse("{}").unwrap(),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let rendered = host
            .dispatch_client_ui_control(
                host_generated_json(json!({
                    "subtype":"ui_render",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"utf16-pane",
                    "props":{},
                    "viewport":{"columns":80,"rows":24},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let revision = rendered.render_revision.unwrap();
        let wire_tree = rendered.response.to_json_string().unwrap();
        assert!(wire_tree.contains(r#""key":"\ud800""#));
        let site = client_site_from_input(&json!({"component":"Pane","instance_id":"utf16-pane"}))
            .unwrap();
        let identity = lock(&host.client_ui.data)
            .parents
            .get(&site)
            .and_then(|parent| parent.clients.keys().next())
            .cloned()
            .expect("the parent render records its Client identity");
        assert_eq!(identity.key_units(), &[0xd800]);

        let mut mount = host_generated_json(json!({
            "type":"mount",
            "surface":"desktop",
            "component":"Pane",
            "instance_id":"utf16-pane",
            "plugin":"utf16",
            "client":"�",
            "module":"card.tsx",
            "render_revision":revision,
            "columns":80,
            "rows":24,
        }));
        set_exact_string_field(&mut mount, "client", vec![0xd800]).unwrap();
        let mounted = host
            .dispatch_client_ui_operation(mount, session.as_ref())
            .await
            .unwrap();
        let runtime_id = test_value(&mounted)["runtimeId"]
            .as_str()
            .unwrap()
            .to_owned();

        let key = Utf16JsonProjection::root_string("�".to_owned(), vec![0xd800]).unwrap();
        let client = Utf16JsonProjection::object(vec![
            ("plugin".into(), host_generated_json(json!("utf16"))),
            ("key".into(), key),
            ("module".into(), host_generated_json(json!("card.tsx"))),
        ])
        .unwrap();
        let mut commit = host_generated_json(json!({
            "type":"draw_commit",
            "surface":"desktop",
            "component":"Pane",
            "instance_id":"utf16-pane",
            "render_revision":revision,
            "clients":[],
        }));
        commit
            .set_field("clients", Utf16JsonProjection::array(vec![client]).unwrap())
            .unwrap();
        let committed = host
            .dispatch_client_ui_operation(commit, session.as_ref())
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);

        let frame =
            wait_for_exact_client_frame(&session_impl, &runtime_id, "UTF-16 ui.message", |frame| {
                projection_tree_contains_text(frame, "exact-utf16")
            })
            .await;
        assert!(projection_tree_contains_text(&frame, "exact-utf16"));
    }

    #[test]
    fn parent_press_actions_are_recovered_only_from_validated_render_tree() {
        let site = ParentControlSite {
            surface: "desktop".into(),
            component: ClientUiComponent::Pane,
            request_id: "pane-1".encode_utf16().collect(),
        };
        let tree = Utf16JsonProjection::from_serde_value(&json!({
            "type":"Box",
            "props":{},
            "children":[{
                "type":"Select",
                "props":{"key":"choice","options":[{"value":"a"},{"value":"b"}]},
                "press":{"plugin":"review","handle":7},
                "children":[]
            }]
        }))
        .unwrap();
        let actions = collect_parent_press_actions(&tree, &site).unwrap();

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].site, site);
        assert_eq!(
            actions[0].plugin,
            "review".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(actions[0].handle, 7);
        assert_eq!(
            actions[0].element.as_deref(),
            Some("choice".encode_utf16().collect::<Vec<_>>().as_slice())
        );
        assert_eq!(actions[0].kind, "Select");
        assert!(actions[0]
            .select_values
            .contains(&"a".encode_utf16().collect::<Vec<_>>()));
        assert!(actions[0]
            .select_values
            .contains(&"b".encode_utf16().collect::<Vec<_>>()));
    }

    #[test]
    fn markdown_press_action_keeps_the_parent_links_for_native_preflight() {
        let site = ParentControlSite {
            surface: "desktop".into(),
            component: ClientUiComponent::Pane,
            request_id: "pane-1".encode_utf16().collect(),
        };
        let tree = Utf16JsonProjection::from_serde_value(&json!({
            "type":"Markdown",
            "props":{"text":"link","key":"doc","pressableLinks":["https://example.com/a"]},
            "press":{"plugin":"review","handle":8},
        }))
        .unwrap();
        let actions = collect_parent_press_actions(&tree, &site).unwrap();

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "Markdown");
        let expected_links = vec!["https://example.com/a".encode_utf16().collect::<Vec<_>>()];
        assert_eq!(
            actions[0].pressable_links.as_deref(),
            Some(expected_links.as_slice())
        );
    }

    #[test]
    fn worker_fault_result_is_a_local_fault_union_with_native_reason_cap() {
        let response = worker_fault_response(
            Some("runtime-1"),
            4,
            &ExactHookText::from_text("plugin"),
            &ExactHookText::from_text("view.ts"),
            "render",
            &ExactHookText::from_text("😀".repeat(120)),
        );
        assert_eq!(
            response
                .at_pointer("/handled")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(response.number_at("/renderRevision"), Some(4.0));
        assert_eq!(response.string_at("/runtimeId"), Some("runtime-1"));
        assert_eq!(response.string_at("/fault/phase"), Some("render"));
        assert_eq!(response.string_at("/fault/source"), Some("worker"));
        assert!(response
            .string_units("/fault/reason")
            .is_some_and(|units| units.len() <= 200));
    }

    #[tokio::test]
    async fn worker_frames_preserve_only_positive_safe_frame_sequences() {
        let host = ModHost::start(None).await.unwrap();
        let result = json!({
            "tree":{"type":"Text","props":{},"children":["ready"]},
            "hasPointerListener":false,
            "hasKeyListener":false,
            "frameSequence":3,
        });
        let frame = host
            .frame_for_worker_result("runtime-1", 4, "plugin", &host_generated_json(result))
            .unwrap();
        assert_eq!(test_value(&frame)["frameSequence"], 3);
        assert_eq!(test_value(&frame)["renderRevision"], 4);
        assert!(host
            .frame_for_worker_result(
                "runtime-1",
                4,
                "plugin",
                &host_generated_json(json!({
                    "tree":{"type":"Text","props":{},"children":["stale"]},
                    "hasPointerListener":false,
                    "hasKeyListener":false,
                    "frameSequence":0,
                })),
            )
            .is_none());
        assert!(
            host.frame_for_worker_result(
                "runtime-1",
                4,
                "plugin",
                &host_generated_json(json!({
                    "tree":{"type":"Markdown","props":{"key":"links","text":"docs"},"held":8},
                    "hasPointerListener":false,
                    "hasKeyListener":false,
                    "frameSequence":3,
                })),
            )
            .is_none(),
            "Client Markdown cannot carry a private held handle"
        );
        assert!(host
            .frame_for_worker_result(
                "runtime-1",
                4,
                "plugin",
                &host_generated_json(json!({
                    "tree":{"type":"Text","props":{},"children":["missing"]},
                    "hasPointerListener":false,
                    "hasKeyListener":false,
                })),
            )
            .is_none());
    }

    #[test]
    fn client_tree_projection_lowers_private_handles_and_rejects_markdown_handles() {
        let tree = Utf16JsonProjection::parse(
            r#"{"type":"Box","props":{"label":"\ud800"},"children":[[{"type":"Input","props":{"key":"q\ud800","value":"v\ud800"},"held":7,"children":[]}],{"type":"Markdown","props":{"key":"links","text":"docs","pressableLinks":["https://example.test"]},"children":[]},{"type":"Markdown","props":{"key":"plain","text":"plain"},"children":[]},{"type":"Text","props":{},"children":["text:\ud800"]}]}"#,
        )
        .unwrap();

        let public = public_client_tree_projection(&tree, "review").unwrap();
        assert_eq!(public.string_units("/props/label"), Some(vec![0xd800]));
        assert_eq!(
            public.string_units("/children/0/0/props/key"),
            Some(vec![u16::from(b'q'), 0xd800])
        );
        assert_eq!(
            public.string_units("/children/0/0/props/value"),
            Some(vec![u16::from(b'v'), 0xd800])
        );
        assert_eq!(
            public.string_units("/children/3/children/0"),
            Some("text:".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            public.string_at("/children/0/0/press/plugin"),
            Some("review")
        );
        assert_eq!(public.number_at("/children/0/0/press/handle"), Some(7.0));
        assert_eq!(
            public.string_units("/children/1/props/pressableLinks/0"),
            Some("https://example.test".encode_utf16().collect())
        );
        assert!(public.subprojection("/children/0/0/held").is_err());
        assert!(public.subprojection("/children/1/press").is_err());
        assert!(public.subprojection("/children/1/held").is_err());
        assert!(public.subprojection("/children/2/press").is_err());
        assert!(public.subprojection("/children/2/held").is_err());
        let input_node = public.subprojection("/children/0/0").unwrap();
        let input_fields = input_node
            .object_entries()
            .unwrap()
            .into_iter()
            .map(|field| field.code_units)
            .collect::<Vec<_>>();
        assert_eq!(
            input_fields,
            ["type", "props", "press", "children"]
                .into_iter()
                .map(|key| key.encode_utf16().collect())
                .collect::<Vec<Vec<u16>>>()
        );
        assert!(public
            .to_json_string()
            .unwrap()
            .contains(r#""label":"\ud800""#));

        let invalid_handle =
            Utf16JsonProjection::parse(r#"{"type":"Button","props":{},"held":1.5}"#).unwrap();
        assert!(public_client_tree_projection(&invalid_handle, "review").is_none());
        let conflicting_press = Utf16JsonProjection::parse(
            r#"{"type":"Button","props":{},"held":1,"press":{"plugin":"other","handle":9}}"#,
        )
        .unwrap();
        assert!(public_client_tree_projection(&conflicting_press, "review").is_none());
        let markdown_held =
            Utf16JsonProjection::parse(r#"{"type":"Markdown","props":{"text":"docs"},"held":8}"#)
                .unwrap();
        assert!(public_client_tree_projection(&markdown_held, "review").is_none());
        let markdown_press = Utf16JsonProjection::parse(
            r#"{"type":"Markdown","props":{"text":"docs"},"press":{"plugin":"review","handle":8}}"#,
        )
        .unwrap();
        assert!(public_client_tree_projection(&markdown_press, "review").is_none());
    }

    #[test]
    fn client_frame_identity_keeps_the_parent_and_client_source_units() {
        let site = ClientUiRenderSite::new(
            ClientUiComponent::AbovePrompt,
            ExactHookText::from_utf16(vec![u16::from(b'i'), 0xd800]),
        )
        .unwrap();
        let identity = ClientUiIdentity::new(
            ExactHookText::from("plugin"),
            ExactHookText::from_utf16(vec![u16::from(b'm'), 0xd800]),
            vec![u16::from(b'k'), 0xd800],
        )
        .unwrap();
        let (parent, client) =
            client_frame_identity_projections(&ClientUiInstanceKey { site, identity });

        assert_eq!(parent.string_at("/surface"), Some("desktop"));
        assert_eq!(parent.string_at("/component"), Some("AbovePrompt"));
        assert_eq!(
            parent.string_units("/instance_id"),
            Some(vec![u16::from(b'i'), 0xd800])
        );
        assert_eq!(client.string_at("/plugin"), Some("plugin"));
        assert_eq!(
            client.string_units("/key"),
            Some(vec![u16::from(b'k'), 0xd800])
        );
        assert_eq!(
            client.string_units("/module"),
            Some(vec![u16::from(b'm'), 0xd800])
        );
    }

    // This session intentionally inherits the default unavailable frame sink.
    // The callback's model API is an independently observable side effect.
    struct ReceiptTestSession {
        root: std::path::PathBuf,
        models: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ModSessionContext for ReceiptTestSession {
        fn cwd(&self) -> std::path::PathBuf { self.root.clone() }
        fn root(&self) -> std::path::PathBuf { self.root.clone() }
        async fn model(&self) -> String {
            self.models.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            "observed callback".into()
        }
        async fn id(&self) -> String { "receipt-test".into() }
        async fn turns(&self) -> u64 { 0 }
    }

    struct ReceiptAcceptedSession {
        inner: Arc<ReceiptTestSession>,
        frames: std::sync::Mutex<Vec<(String, Utf16JsonProjection)>>,
        refuse_fault: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl ModSessionContext for ReceiptAcceptedSession {
        fn cwd(&self) -> std::path::PathBuf { self.inner.cwd() }
        fn root(&self) -> std::path::PathBuf { self.inner.root() }
        async fn model(&self) -> String { self.inner.model().await }
        async fn id(&self) -> String { self.inner.id().await }
        async fn turns(&self) -> u64 { self.inner.turns().await }
        async fn emit_mod_ui_client_frame(&self, runtime: &str, frame: &Utf16JsonProjection) -> Result<(), ModError> {
            if frame.at_pointer("/fault").is_some() && self.refuse_fault.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ModError::Unavailable("injected fault admission refusal".into()));
            }
            self.frames.lock().unwrap().push((runtime.into(), frame.clone()));
            Ok(())
        }
    }

    async fn real_worker_frame_receipt(accept: bool) {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(&child, r#"
          import { Box, Button, Text, h } from 'claude:surface-runtime';
          export function Card(props, surface) {
            return h(Box, {children:[
              h(Text, {children: props.title}),
              h(Button, {key: 'send', label: 'Send',
                onPress: () => surface.post({ title: 'callback result' })})
            ]});
          }
        "#).unwrap();
        std::fs::write(&entry, r#"
          export function register(on) {
            on('ui.render', {surface:'desktop', component:'Pane'}, ($, event) => {
              const {Client, h} = $.ui.resolve(event);
              return h(Client, {key:'card', module:'./card.tsx', props:{title:'initial'}});
            });
            on('ui.message', async ($, event) => {
              await $.session.model();
              return {handled:true, props:{title:event.data.title}};
            });
          }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        let observation = Arc::new(ReceiptTestSession {
            root: directory.path().to_path_buf(), models: std::sync::atomic::AtomicUsize::new(0),
        });
        let accepting = Arc::new(ReceiptAcceptedSession {
            inner: observation.clone(), frames: std::sync::Mutex::new(Vec::new()),
            refuse_fault: std::sync::atomic::AtomicBool::new(false),
        });
        let session: Arc<dyn ModSessionContext> = if accept { accepting.clone() } else { observation.clone() };
        host.attach_background_context(Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "review", "review@user", directory.path(), &entry,
            Utf16JsonProjection::parse("{}").unwrap(), "user", None, Some(&prepared),
        ).await.unwrap();
        let render = host.dispatch_client_ui_control(host_generated_json(json!({
            "subtype":"ui_render", "surface":"desktop", "component":"Pane",
            "instance_id":"pane-1", "props":{}, "viewport":{"columns":80,"rows":24},
        })), session.as_ref()).await.unwrap();
        let revision = render.render_revision.unwrap();
        let mount = host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"mount", "surface":"desktop", "component":"Pane", "instance_id":"pane-1",
            "plugin":"review", "client":"card", "module":"card.tsx", "render_revision":revision,
            "columns":80, "rows":24,
        })), session.as_ref()).await.unwrap();
        let mounted = test_value(&mount);
        let runtime_id = mounted["runtimeId"].as_str().unwrap().to_owned();
        let handle = tree_press_handle_for_key(&mounted["tree"], "send").unwrap();
        let commit = host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"draw_commit", "surface":"desktop", "component":"Pane", "instance_id":"pane-1",
            "render_revision":revision, "clients":[{"plugin":"review","key":"card","module":"card.tsx"}],
        })), session.as_ref()).await.unwrap();
        assert_eq!(test_value(&commit)["handled"], true);
        let result = tokio::time::timeout(Duration::from_secs(5), host.dispatch_client_ui_operation(
            host_generated_json(json!({"type":"runHeld", "runtimeId":runtime_id,
              "render_revision":revision, "handle":handle})), session.as_ref(),
        )).await.expect("post settlement must finish");
        assert_eq!(observation.models.load(std::sync::atomic::Ordering::SeqCst), 1,
            "the real callback API ran exactly once even when frame admission fails");
        if accept {
            let response = test_value(&result.unwrap());
            assert!(tree_contains_text(&response["tree"], "callback result"));
            {
                let frames = accepting.frames.lock().unwrap();
                assert!(frames.iter().any(|(runtime, frame)| runtime == &runtime_id &&
                    projection_tree_contains_text(frame, "callback result")));
            }
            // A refused fault snapshot must remain retryable even though the
            // distinct ui.fault hook may already have been dispatched.
            accepting.refuse_fault.store(true, std::sync::atomic::Ordering::SeqCst);
            let generation = lock(&host.client_ui.data).instances[&runtime_id].worker_generation.unwrap();
            let fault = host_generated_json(json!({
                "kind":"ui.client.event", "runtimeId":runtime_id, "generation":generation,
                "plugin":"review", "element":"card", "module":"card.tsx",
                "action":"fault", "phase":"run", "reason":"injected fault",
            }));
            host.handle_client_ui_background_event(&fault, session.as_ref()).await.unwrap();
            assert_eq!(lock(&host.client_ui.data).instances[&runtime_id].worker_fault_snapshot,
                ClientFaultSnapshotState::Pending);
            assert!(lock(&host.client_ui.data).instances[&runtime_id].worker_fault_event_dispatched);
            let claim = host.claim_client_worker_fault_snapshot(&runtime_id).unwrap();
            assert!(host.claim_client_worker_fault_snapshot(&runtime_id).is_none(),
                "an in-flight publication excludes a duplicate");
            drop(claim); // Simulates cancellation while awaiting queue admission.
            assert_eq!(lock(&host.client_ui.data).instances[&runtime_id].worker_fault_snapshot,
                ClientFaultSnapshotState::Pending);
            accepting.refuse_fault.store(false, std::sync::atomic::Ordering::SeqCst);
            host.handle_client_ui_background_event(&fault, session.as_ref()).await.unwrap();
            host.handle_client_ui_background_event(&fault, session.as_ref()).await.unwrap();
            assert_eq!(lock(&host.client_ui.data).instances[&runtime_id].worker_fault_snapshot,
                ClientFaultSnapshotState::Sent);
            assert_eq!(accepting.frames.lock().unwrap().iter().filter(|(runtime, frame)|
                runtime == &runtime_id && frame.at_pointer("/fault").is_some()).count(), 1);
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("outcome is unknown"), "{error}");
            assert!(accepting.frames.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn real_worker_callback_with_default_sink_has_unknown_outcome() {
        real_worker_frame_receipt(false).await;
    }
    #[tokio::test]
    async fn real_worker_callback_with_accepting_sink_returns_updated_frame() {
        real_worker_frame_receipt(true).await;
    }


    async fn real_worker_async_input_waits_for_post_and_fences_rejection(reject: bool) {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        let fail = if reject { "throw new Error('failed after update');" } else { "" };
        std::fs::write(&child, r#"
          import { Input, h } from 'claude:surface-runtime';
          let latest = '';
          export function Card(props, surface) {
            latest = props.value;
            return h(Input, {key:'query', value:latest, onSubmit() {},
              onInput: async value => {
                await new Promise(resolve => {
                  const stop = surface.every(35, () => { stop(); resolve(); });
                });
                surface.post({value});
                // Completion depends on the Host dispatching this post and
                // updating props while the held operation is still active.
                await new Promise(resolve => {
                  const stop = surface.every(2, () => { if (latest === value) { stop(); resolve(); } });
                });
                // REJECT_IF_REQUESTED
              }});
          }
        "#.replace("// REJECT_IF_REQUESTED", fail)).unwrap();
        std::fs::write(&entry, r#"
          export function register(on) {
            on('ui.render', {surface:'desktop', component:'Pane'}, ($, event) => {
              const {Box, Client, h} = $.ui.resolve(event);
              return h(Box, {},
                h(Client, {key:'card', module:'./card.tsx', props:{value:''}}),
                h(Client, {key:'sibling', module:'./card.tsx', props:{value:''}}),
              );
            });
            on('ui.message', async ($, event) => {
              await $.session.model();
              return {handled:true, props:{value:event.data.value}};
            });
          }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let session_impl = Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            model_gate: StdMutex::new(Some(UiLifecycleModelGate {
                entered: entered.clone(), release: release.clone(), completed: completed.clone(),
            })),
            ..Default::default()
        });
        let session: Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "review", "review@user", directory.path(), &entry,
            Utf16JsonProjection::parse("{}").unwrap(), "user", None, Some(&prepared),
        ).await.unwrap();
        let render = host.dispatch_client_ui_control(host_generated_json(json!({
            "subtype":"ui_render", "surface":"desktop", "component":"Pane",
            "instance_id":"pane-async", "props":{}, "viewport":{"columns":80,"rows":24},
        })), session.as_ref()).await.unwrap();
        let revision = render.render_revision.unwrap();
        let mount = host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"mount", "surface":"desktop", "component":"Pane", "instance_id":"pane-async",
            "plugin":"review", "client":"card", "module":"card.tsx", "render_revision":revision,
            "columns":80, "rows":24,
        })), session.as_ref()).await.unwrap();
        let mounted = test_value(&mount);
        let runtime_id = mounted["runtimeId"].as_str().unwrap().to_owned();
        let input = tree_node_with_key(&mounted["tree"], "query").unwrap();
        assert_eq!(input["props"]["value"], "");
        let handle = tree_press_handle_for_key(&mounted["tree"], "query").unwrap();
        let sibling = host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"mount", "surface":"desktop", "component":"Pane", "instance_id":"pane-async",
            "plugin":"review", "client":"sibling", "module":"card.tsx", "render_revision":revision,
            "columns":80, "rows":24,
        })), session.as_ref()).await.unwrap();
        let sibling_id = test_value(&sibling)["runtimeId"].as_str().unwrap().to_owned();
        {
            let data = lock(&host.client_ui.data);
            let first_env = &data.instances[&runtime_id].worker_environment_id;
            assert!(first_env.is_some());
            assert_eq!(first_env, &data.instances[&sibling_id].worker_environment_id,
                "the two actual Worker surfaces share the same environment");
        }
        host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"draw_commit", "surface":"desktop", "component":"Pane", "instance_id":"pane-async",
            "render_revision":revision, "clients":[{"plugin":"review","key":"card","module":"card.tsx"},
                {"plugin":"review","key":"sibling","module":"card.tsx"}],
        })), session.as_ref()).await.unwrap();
        let operation = host_generated_json(json!({"type":"runHeld", "runtimeId":runtime_id,
            "render_revision":revision, "handle":handle, "event":{"kind":"change","value":"typed"}}));
        let host_task = host.clone();
        let session_task = session.clone();
        let mut task = tokio::spawn(async move {
            host_task.dispatch_client_ui_operation(operation, session_task.as_ref()).await
        });
        if tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.is_err() {
            eprintln!("TRACE action finished={} logs={:?} frames={:?}", task.is_finished(),
                lock(&session_impl.logs), lock(&session_impl.frames));
            if task.is_finished() { eprintln!("TRACE result {:?}", task.await); }
            panic!("the post reaches the real Host callback after the first await");
        }
        assert!(tokio::time::timeout(Duration::from_millis(60), &mut task).await.is_err(),
            "runHeld must not confirm the stale Input while its post is held");
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), completed.notified()).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), &mut task).await
            .expect("held callback and its post must finish without queue deadlock")
            .unwrap().unwrap();
        let response = test_value(&response);
        if reject {
            assert_eq!(response["outcome"], json!({"status":"unknown","runtime":"unavailable"}));
            assert_eq!(response["fault"]["phase"], "run");
            assert!(response["fault"]["reason"].as_str().unwrap().contains("failed after update"));
            {
                let data = lock(&host.client_ui.data);
                assert!(data.instances[&runtime_id].worker_fault.is_some());
                assert!(data.instances[&sibling_id].worker_fault.is_some(),
                    "the shared sibling is fenced before the structured unknown result returns");
            }
            let sibling_render = host.dispatch_client_ui_operation(host_generated_json(json!({
                "type":"render", "runtimeId":sibling_id, "render_revision":revision,
            })), session.as_ref()).await.unwrap();
            assert_eq!(test_value(&sibling_render)["handled"], false);
            let next = host.dispatch_client_ui_operation(host_generated_json(json!({
                "type":"runHeld", "runtimeId":runtime_id, "render_revision":revision, "handle":handle,
            })), session.as_ref()).await.unwrap();
            assert_eq!(test_value(&next)["handled"], false);
        } else {
            let input = tree_node_with_key(&response["tree"], "query").unwrap();
            assert_eq!(input["props"]["value"], "typed");
            assert!(lock(&session_impl.frames).iter().any(|(runtime, frame)| {
                runtime == &runtime_id && tree_node_with_key(&frame["tree"], "query")
                    .is_some_and(|node| node["props"]["value"] == "typed")
            }), "the post's updated frame was admitted before runHeld returned");
        }
    }

    #[tokio::test]
    async fn real_worker_async_input_waits_for_post_before_confirming() {
        real_worker_async_input_waits_for_post_and_fences_rejection(false).await;
    }

    #[tokio::test]
    async fn real_worker_async_input_rejection_returns_unknown_and_fences_runtime() {
        real_worker_async_input_waits_for_post_and_fences_rejection(true).await;
    }


    #[tokio::test]
    async fn real_worker_held_drop_after_callback_result_fences_before_next_action() {
        real_worker_held_drop_fences_before_next_action(false).await;
    }

    #[tokio::test]
    async fn real_worker_held_drop_after_finalize_ack_revokes_worker_environment() {
        real_worker_held_drop_fences_before_next_action(true).await;
    }

    async fn real_worker_held_drop_fences_before_next_action(after_finalize_ack: bool) {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(&child, r#"
          import { Box, Text, Button, h } from 'claude:surface-runtime';
          export function Card(props, surface) {
            return h(Box, {children:[h(Text, {children:props.title}),
              h(Button, {key:'send', label:'Send', onPress:() => surface.post({title:'after effect'})})]});
          }
        "#).unwrap();
        std::fs::write(&entry, r#"
          export function register(on) {
            on('ui.render', {surface:'desktop', component:'Pane'}, ($, event) => {
              const {Client, h} = $.ui.resolve(event);
              return h(Client, {key:'card', module:'./card.tsx', props:{title:'initial'}});
            });
            on('ui.message', async ($, event) => {
              await $.session.model();
              return {handled:true, props:{title:event.data.title}};
            });
          }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        let session_impl = Arc::new(UiLifecycleSession {root:directory.path().to_path_buf(), ..Default::default()});
        let session: Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared("review", "review@user", directory.path(), &entry,
            Utf16JsonProjection::parse("{}").unwrap(), "user", None, Some(&prepared)).await.unwrap();
        let render = host.dispatch_client_ui_control(host_generated_json(json!({
            "subtype":"ui_render", "surface":"desktop", "component":"Pane", "instance_id":"drop-test",
            "props":{}, "viewport":{"columns":80,"rows":24},
        })), session.as_ref()).await.unwrap();
        let revision = render.render_revision.unwrap();
        let mount = host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"mount", "surface":"desktop", "component":"Pane", "instance_id":"drop-test",
            "plugin":"review", "client":"card", "module":"card.tsx", "render_revision":revision,
            "columns":80, "rows":24,
        })), session.as_ref()).await.unwrap();
        let mounted = test_value(&mount);
        let runtime_id = mounted["runtimeId"].as_str().unwrap().to_owned();
        let (old_environment, generation) = {
            let data = lock(&host.client_ui.data);
            let instance = &data.instances[&runtime_id];
            (instance.worker_environment_id.clone().unwrap(), instance.worker_generation.unwrap())
        };
        let handle = tree_press_handle_for_key(&mounted["tree"], "send").unwrap();
        host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"draw_commit", "surface":"desktop", "component":"Pane", "instance_id":"drop-test",
            "render_revision":revision, "clients":[{"plugin":"review","key":"card","module":"card.tsx"}],
        })), session.as_ref()).await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        if after_finalize_ack {
            *lock(&host.client_ui.finalize_gate) = Some(ClientHeldFinalizeGate {
                entered: entered.clone(), release: release.clone(),
            });
        } else {
            let delivered = Arc::new(tokio::sync::Notify::new());
            *lock(&session_impl.frame_gate) = Some(UiLifecycleFrameGate {
                runtime_id:runtime_id.clone(), expected_text:"after effect",
                entered:entered.clone(), release:release.clone(), delivered,
            });
        }
        let host_task = host.clone();
        let session_task = session.clone();
        let operation = host_generated_json(json!({"type":"runHeld", "runtimeId":runtime_id,
            "render_revision":revision, "handle":handle}));
        let task = tokio::spawn(async move {
            host_task.dispatch_client_ui_operation(operation, session_task.as_ref()).await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while host.client_ui.held_result_received.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        }).await.expect("the Worker callback result was already received before dropping the Host future");
        assert!(!task.is_finished(), "the selected Host completion boundary is held");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(lock(&host.client_ui.data).instances[&runtime_id].worker_fault.is_some(),
            "dropping an unfinalized lease synchronously fences the Host runtime");
        release.notify_one();
        let next = host.dispatch_client_ui_operation(host_generated_json(json!({
            "type":"render", "runtimeId":runtime_id, "render_revision":revision,
        })), session.as_ref()).await.unwrap();
        assert_eq!(test_value(&next)["handled"], false);
        let _fault = wait_for_client_frame(&session_impl, &runtime_id, "cancelled lease fault", |frame| {
            frame.get("fault").is_some()
        }).await;
        if after_finalize_ack {
            let worker_result = host.run_client_worker_operation(host_generated_json(json!({
                "type":"render", "runtimeId":runtime_id, "generation":generation,
            }))).await.unwrap();
            assert_eq!(test_value(&worker_result), json!({"stale":true}),
                "the late cancellation revoked the actual Worker VM, not only the Host state");
            let remount = host.dispatch_client_ui_operation(host_generated_json(json!({
                "type":"mount", "surface":"desktop", "component":"Pane", "instance_id":"drop-test",
                "plugin":"review", "client":"card", "module":"card.tsx", "render_revision":revision,
                "columns":80, "rows":24,
            })), session.as_ref()).await.unwrap();
            let new_runtime = test_value(&remount)["runtimeId"].as_str().unwrap().to_owned();
            let data = lock(&host.client_ui.data);
            assert_ne!(data.instances[&new_runtime].worker_environment_id.as_deref(), Some(old_environment.as_str()),
                "remount must create a fresh VM after a lost finalize acknowledgement");
        } else {
            assert!(!lock(&session_impl.frames).iter().any(|(id, frame)| {
                id == &runtime_id && tree_contains_text(&frame["tree"], "after effect")
            }), "a dropped frame admission cannot later appear as an acknowledged frame");
        }
    }

    #[tokio::test]
    async fn prepared_tsx_client_runs_the_host_render_mount_draw_post_and_unload_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(
            &child,
            r#"
              import { Box, Text, Button, Input, Select, Markdown, h } from 'claude:surface-runtime';
              type CardProps = { title: string; fail?: boolean; failRun?: boolean; timerFault?: boolean };
              export function Card(props: CardProps, surface: any) {
                if (props.fail) throw new Error('client render exploded');
                if (props.timerFault && !surface.state.timerStarted) {
                  surface.setState({ timerStarted: true });
                  surface.every(250, () => { throw new Error('client timer exploded'); });
                }
                if (!surface.state.posted) {
                  surface.setState({ posted: true });
                  if (!props.failRun && !props.timerFault) surface.post({ title: 'posted' });
                }
                return h(Box, { flexDirection: 'column', children: [
                  h(Text, { key: 'title', children: props.title }),
                  h(Button, { key: 'submit', label: 'Send',
                    onPress: () => {
                      if (props.failRun) throw new Error('client press exploded');
                      surface.post({ title: 'pressed' });
                    } }),
                  h(Input, {
                    key: 'query', value: '',
                    onSubmit: (value, event) => surface.post({ title: `Input submitted (${event.kind}): ${value}` }),
                    onInput: (value, event) => {
                      if (value === 'cancel-block') {
                        surface.post({ block: true });
                        return;
                      }
                      if (value === 'after-cancel') {
                        surface.post({ afterCancellation: true });
                        return;
                      }
                      if (value === 'barrier') {
                        surface.post({ title: 'BARRIER UPPERCASE' });
                        surface.post({ title: '' });
                        surface.post({ noProps: true });
                        return;
                      }
                      if (value.startsWith('host-removal-')) {
                        surface.post({ hostRemovalRace: value });
                        return;
                      }
                      surface.post({ title: `Input changed (${event.kind}): ${value}` });
                    },
                  }),
                  h(Select, { key: 'mode', value: 'fast', options: [{ value: 'fast' }],
                    onSelect: (value, event) => surface.post({ title: `Select ${event.value}: ${value}` }) }),
                  h(Markdown, { key: 'links', text: '[docs](https://example.test)',
                    pressableLinks: ['https://example.test'] }),
                ] });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
              let blockedPostAborted = false;
              let releaseBlockedPostAborted;
              const blockedPostAbortedSignal = new Promise(resolve => {
                releaseBlockedPostAborted = resolve;
              });
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
                  const { Client, h } = $.ui.resolve(event);
                  const clientProps = event.props.invalid
                    ? { values: Array.from({ length: 20001 }, () => 0) }
                    : event.props.fail
                      ? { title: 'will fail', fail: true }
                    : event.props.failRun
                      ? { title: 'ready to fail', failRun: true }
                      : event.props.timerFault
                        ? { title: 'timer waiting', timerFault: true }
                      : { title: 'initial' };
                  const client = h(Client, {
                    key: 'card', module: './card.tsx', props: clientProps,
                  });
                  return client;
                });
                on('ui.message', async (_$, event, next) => {
                  if (event.data.hostRemovalRace) {
                    if (event.data.hostRemovalRace !== 'host-removal-during-frame') {
                      await _$.session.model();
                    }
                    return event.data.hostRemovalRace === 'host-removal-no-props'
                      ? { handled: true }
                      : { handled: true, props: { title: event.data.hostRemovalRace } };
                  }
                  if (event.data.block) {
                    _$.ui.log('blocking ui.message reached');
                    await new Promise(resolve => {
                      next.signal.addEventListener('abort', () => {
                        blockedPostAborted = true;
                        releaseBlockedPostAborted();
                        resolve();
                      }, { once: true });
                    });
                    return { handled: true, props: { title: 'late props' } };
                  }
                  if (event.data.afterCancellation) {
                    await blockedPostAbortedSignal;
                    return { handled: true, props: {
                      title: blockedPostAborted ? 'after cancellation' : 'not cancelled',
                    } };
                  }
                  return event.data.noProps
                    ? { handled: true }
                    : { handled: true, props: { title: event.data.title } };
                });
                on('ui.press', (_$, event, next) => next(event));
                on('ui.input', (_$, event, next) => next({ ...event, value: event.value.trim() }));
                on('ui.select', (_$, event, next) => next({ ...event, value: `selected:${event.value}` }));
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));

        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "review",
            "review@user",
            directory.path(),
            &entry,
            Utf16JsonProjection::parse("{}").unwrap(),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let render = |props: Value| {
            json!({
                "subtype":"ui_render",
                "surface":"desktop",
                "component":"Pane",
                "instance_id":"pane-1",
                "props":props,
                "viewport":{"columns":80,"rows":24},
            })
        };

        // The Rust Host's stricter bounded parent-tree check is downstream of
        // the worker's generic tree check. An oversized Client payload must
        // restore the original engine answer and original input props without
        // entering the Client mount/fault lifecycle.
        let original_props = json!({"invalid":true,"preserve":"original"});
        let invalid = host
            .dispatch_client_ui_control(
                host_generated_json(render(original_props.clone())),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&invalid.response)["tree"]["type"], "engine");
        assert_eq!(
            test_value(&invalid.response)["tree"]["ref"],
            invalid.render_revision.unwrap()
        );
        assert_eq!(test_value(&invalid.response)["props"], original_props);
        assert_eq!(test_value(&invalid.response)["rewritten"], false);
        assert!(lock(&session_impl.invalidations).is_empty());
        assert_eq!(host.ui_render_generation(), 0);

        let initial_render = host
            .dispatch_client_ui_control(host_generated_json(render(json!({}))), session.as_ref())
            .await
            .unwrap();
        let initial_epoch = initial_render.client_runtime_epochs["review"];
        assert!(initial_epoch > 0);
        assert!(test_value(&initial_render.response)
            .get("clientRuntimeEpochs")
            .is_none());
        let first_manifest_hash = test_value(&initial_render.response)["client_modules"]["review"]
            .as_str()
            .unwrap()
            .to_owned();

        // Reinstalling the same exact plugin source/hash is still a real
        // environment reload and receives a new sidecar epoch.
        let reloaded = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "review",
            "review@user",
            directory.path(),
            &entry,
            Utf16JsonProjection::parse("{}").unwrap(),
            "user",
            None,
            Some(&reloaded),
        )
        .await
        .unwrap();
        let reloaded_render = host
            .dispatch_client_ui_control(host_generated_json(render(json!({}))), session.as_ref())
            .await
            .unwrap();
        let reloaded_epoch = reloaded_render.client_runtime_epochs["review"];
        assert!(reloaded_epoch > initial_epoch);
        assert_eq!(
            test_value(&reloaded_render.response)["client_modules"]["review"],
            first_manifest_hash
        );

        // A normal redraw advances renderRevision but keeps the environment
        // epoch stable.
        let rendered = host
            .dispatch_client_ui_control(host_generated_json(render(json!({}))), session.as_ref())
            .await
            .unwrap();
        assert_eq!(rendered.client_runtime_epochs["review"], reloaded_epoch);
        let revision = rendered.render_revision.unwrap();
        let rendered_json = test_value(&rendered.response);
        let client = &rendered_json["tree"];
        let client_module = client["props"]["module"].as_str().unwrap().to_owned();
        assert_eq!(client["type"], "Client");
        assert_eq!(client["client"]["plugin"], "review");
        assert_eq!(client_module, "card.tsx");
        assert!(std::fs::read_to_string(&entry)
            .unwrap()
            .contains("module: './card.tsx'"));
        let manifest = host
            .dispatch_client_ui_control(
                host_generated_json(json!({"subtype":"ui_client_module","plugin":"review"})),
                session.as_ref(),
            )
            .await
            .unwrap()
            .response;
        assert_eq!(test_value(&manifest)["modules"][0]["module"], "card.tsx");

        let mount = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let runtime_id = test_value(&mount)["runtimeId"].as_str().unwrap().to_owned();
        assert_eq!(test_value(&mount)["renderRevision"], revision);
        assert_eq!(test_value(&mount)["frameSequence"], 1);
        assert!(tree_contains_text(&test_value(&mount)["tree"], "initial"));
        let mount_value = test_value(&mount);
        let mounted_tree = &mount_value["tree"];
        let input_node = tree_node_with_key(mounted_tree, "query").unwrap();
        assert_eq!(input_node["type"], "Input");
        assert_eq!(input_node["press"]["plugin"], "review");
        assert!(input_node["press"]["handle"].as_u64().unwrap() > 0);
        assert!(input_node.get("held").is_none());
        assert!(input_node["props"].get("onSubmit").is_none());
        assert!(input_node["props"].get("onInput").is_none());
        assert!(input_node["props"].get("onEvent").is_none());
        for key in ["submit", "mode"] {
            let node = tree_node_with_key(mounted_tree, key)
                .unwrap_or_else(|| panic!("rendered {key} node exists"));
            assert_eq!(node["press"]["plugin"], "review");
            assert!(node["press"]["handle"].as_u64().unwrap() > 0);
            assert!(node.get("held").is_none());
        }
        let select_node = tree_node_with_key(mounted_tree, "mode").unwrap();
        assert!(select_node["props"].get("onSelect").is_none());
        assert!(select_node["props"].get("onEvent").is_none());
        let markdown_node = tree_node_with_key(mounted_tree, "links").unwrap();
        assert!(markdown_node["props"].get("onLinkPress").is_none());
        assert!(markdown_node["props"].get("onEvent").is_none());
        assert_eq!(
            markdown_node["props"]["pressableLinks"],
            json!(["https://example.test"]),
            "Native Client exportTree preserves pressableLinks as plain props"
        );
        assert!(
            markdown_node.get("press").is_none(),
            "Native Client Markdown has no held callback"
        );

        let uncommitted_client_press = host
            .dispatch_client_ui_control(
                host_generated_json(json!({
                    "subtype":"ui_client_press",
                    "plugin":"review",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "client":"card",
                    "module":client_module,
                    "element":"submit",
                    "event":{"type":"press"},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            test_value(&uncommitted_client_press.response)["handled"],
            false,
            "a Client control is not admitted before its draw is committed"
        );

        let committed = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed)["handled"], true);

        // The initial worker post is queued until draw_commit admits this
        // Client. Once replayed it emits a newer frame and replaces the
        // render-scoped callback handles, so wait before taking any handle.
        let posted =
            wait_for_client_frame(&session_impl, &runtime_id, "initial mount post", |frame| {
                tree_contains_text(&frame["tree"], "posted")
            })
            .await;
        assert!(
            posted["frameSequence"].as_u64().unwrap()
                > test_value(&mount)["frameSequence"].as_u64().unwrap()
        );

        let old_input_handle = tree_press_handle_for_key(mounted_tree, "query").unwrap();
        let current_input_handle = tree_press_handle_for_key(&posted["tree"], "query").unwrap();
        assert_ne!(old_input_handle, current_input_handle);
        for handle in [old_input_handle, MAX_JS_SAFE_INTEGER] {
            let rejected = host
                .dispatch_client_ui_operation(
                    host_generated_json(json!({
                        "type":"runHeld",
                        "runtimeId":runtime_id,
                        "render_revision":revision,
                        "handle":handle,
                        "event":{"kind":"change","value":"rejected handle ran"},
                    })),
                    session.as_ref(),
                )
                .await
                .unwrap();
            assert_eq!(
                test_value(&rejected),
                json!({"handled":false,"renderRevision":revision}),
                "old and unknown handles are rejected without producing a frame"
            );
        }

        let client_press = |element: &str, event: Value| {
            host_generated_json(json!({
                "subtype":"ui_client_press",
                "plugin":"review",
                "component":"Pane",
                "instance_id":"pane-1",
                "client":"card",
                "module":client_module,
                "element":element,
                "event":event,
            }))
        };
        let pressed_event = host
            .dispatch_client_ui_control(
                client_press("submit", json!({"type":"press"})),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&pressed_event.response)["handled"], true);
        assert_eq!(
            test_value(&pressed_event.response)["reached"]["element"],
            "submit",
            "the hook event uses the Client child key"
        );

        let changed_event = host
            .dispatch_client_ui_control(
                client_press(
                    "query",
                    json!({"type":"input","kind":"change","value":"  typed  "}),
                ),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&changed_event.response)["handled"], true);
        assert_eq!(
            test_value(&changed_event.response)["reached"]["element"],
            "query"
        );
        assert_eq!(
            test_value(&changed_event.response)["reached"]["value"],
            "typed"
        );
        let input_handle = tree_press_handle_for_key(&posted["tree"], "query")
            .expect("rendered Input exposes its public press handle");
        let changed_callback = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"runHeld",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "handle":input_handle,
                    "event":{"kind":"change","value":test_value(&changed_event.response)["reached"]["value"]},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(test_value(&changed_callback).get("tree").is_some());
        let changed_input_frame =
            wait_for_client_frame(&session_impl, &runtime_id, "Input onInput post", |frame| {
                tree_contains_text(&frame["tree"], "Input changed (change): typed")
            })
            .await;
        assert!(!lock(&session_impl.frames)
            .iter()
            .any(|(frame_runtime, frame)| {
                frame_runtime == &runtime_id
                    && tree_contains_text(
                        &frame["tree"],
                        "Input changed (change): rejected handle ran",
                    )
            }));
        assert!(tree_press_handle_for_key(&changed_input_frame["tree"], "query").is_some());
        // The background `ui.message` frame above is an intermediate frame.
        // runHeld settles its synchronous posts and then returns the current
        // render with freshly-issued handles; use that authoritative response
        // for the next operation.
        let barrier_handle =
            tree_press_handle_for_key(&test_value(&changed_callback)["tree"], "query").unwrap();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let delivered = std::sync::Arc::new(tokio::sync::Notify::new());
        *lock(&session_impl.frame_gate) = Some(UiLifecycleFrameGate {
            runtime_id: runtime_id.clone(),
            expected_text: "BARRIER UPPERCASE",
            entered: entered.clone(),
            release: release.clone(),
            delivered: delivered.clone(),
        });
        let barrier_operation = host_generated_json(json!({
            "type":"runHeld",
            "runtimeId":runtime_id,
            "render_revision":revision,
            "handle":barrier_handle,
            "event":{"kind":"change","value":"barrier"},
        }));
        let barrier_host = host.clone();
        let barrier_session = session.clone();
        let barrier_task = tokio::spawn(async move {
            barrier_host
                .dispatch_client_ui_operation(barrier_operation, barrier_session.as_ref())
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .expect("the correlated ui.message reaches its props frame");
        assert!(
            !barrier_task.is_finished(),
            "runHeld waits while synchronous post props are being applied and emitted"
        );
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), delivered.notified())
            .await
            .expect("the intermediate post frame was delivered after release");
        let barrier_callback = barrier_task.await.unwrap().unwrap();
        let barrier_value = test_value(&barrier_callback);
        let barrier_tree = &barrier_value["tree"];
        let title_node = tree_node_with_key(barrier_tree, "title")
            .expect("the final frame contains the title node");
        assert_eq!(
            title_node["children"],
            json!([""]),
            "the synchronous clear post is applied before the runHeld frame returns"
        );
        assert!(
            !tree_contains_text(barrier_tree, "BARRIER UPPERCASE"),
            "the final frame includes later posts and does not return an intermediate props frame"
        );
        assert!(
            lock(&session_impl.frames)
                .iter()
                .any(|(frame_runtime, frame)| {
                    frame_runtime == &runtime_id
                        && tree_contains_text(&frame["tree"], "BARRIER UPPERCASE")
                }),
            "the delayed uppercase props frame was emitted before the clear frame"
        );

        let input_submit_handle = tree_press_handle_for_key(barrier_tree, "query").unwrap();
        let submitted_callback = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"runHeld",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "handle":input_submit_handle,
                    "event":{"kind":"submit","value":"typed"},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(test_value(&submitted_callback).get("tree").is_some());
        let submitted_input_frame =
            wait_for_client_frame(&session_impl, &runtime_id, "Input onSubmit post", |frame| {
                tree_contains_text(&frame["tree"], "Input submitted (submit): typed")
            })
            .await;
        assert!(tree_press_handle_for_key(&submitted_input_frame["tree"], "query").is_some());
        let select_handle =
            tree_press_handle_for_key(&test_value(&submitted_callback)["tree"], "mode")
                .expect("the authoritative runHeld frame exposes the current Select handle");
        let selected_callback = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"runHeld",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "handle":select_handle,
                    "event":{"value":"fast"},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(test_value(&selected_callback).get("tree").is_some());
        let selected_input_frame = wait_for_client_frame(
            &session_impl,
            &runtime_id,
            "Select onSelect post",
            |frame| tree_contains_text(&frame["tree"], "Select fast: fast"),
        )
        .await;
        assert!(tree_press_handle_for_key(&selected_input_frame["tree"], "mode").is_some());

        assert!(
            tree_press_handle_for_key(&selected_input_frame["tree"], "links").is_none(),
            "Native Client Markdown does not expose a held handle"
        );

        let selected_event = host
            .dispatch_client_ui_control(
                client_press("mode", json!({"type":"select","value":"fast"})),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&selected_event.response)["handled"], true);
        assert_eq!(
            test_value(&selected_event.response)["reached"]["element"],
            "mode"
        );
        assert_eq!(
            test_value(&selected_event.response)["reached"]["value"],
            "selected:fast"
        );

        let wrong_client = host
            .dispatch_client_ui_control(
                host_generated_json(json!({
                    "subtype":"ui_client_press",
                    "plugin":"review",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "client":"other-card",
                    "module":client_module,
                    "element":"submit",
                    "event":{"type":"press"},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            test_value(&wrong_client.response)["handled"],
            false,
            "another Client key cannot borrow the committed draw"
        );

        let reset = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"setProps",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "props":{"title":"initial"},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(tree_contains_text(&test_value(&reset)["tree"], "initial"));
        let held = tree_first_press_handle(&test_value(&reset)["tree"])
            .expect("rendered Button has a public press handle");
        let run = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"runHeld",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "handle":held,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(test_value(&run).get("tree").is_some());
        let pressed =
            wait_for_client_frame(&session_impl, &runtime_id, "held Button post", |frame| {
                tree_contains_text(&frame["tree"], "pressed")
            })
            .await;
        assert!(
            pressed["frameSequence"].as_u64().unwrap()
                > test_value(&reset)["frameSequence"].as_u64().unwrap()
        );

        assert!(host
            .active_request_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
        let cancellation_input_handle =
            tree_press_handle_for_key(&test_value(&run)["tree"], "query").unwrap();
        let cancellation_operation = host_generated_json(json!({
            "type":"runHeld",
            "runtimeId":runtime_id,
            "render_revision":revision,
            "handle":cancellation_input_handle,
            "event":{"kind":"change","value":"cancel-block"},
        }));
        let cancellation_host = host.clone();
        let cancellation_session = session.clone();
        let cancellation_task = tokio::spawn(async move {
            cancellation_host
                .dispatch_client_ui_operation(cancellation_operation, cancellation_session.as_ref())
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !host
                    .active_request_epochs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the blocking ui.message hook has an active worker request");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if lock(&session_impl.logs)
                    .iter()
                    .any(|line| line == "blocking ui.message reached")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the real Worker ui.message handler entered before cancellation");
        assert!(!cancellation_task.is_finished());
        host.client_ui
            .fail_post_actions_for_runtime(&runtime_id, "test runtime cancellation");
        let cancellation_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), cancellation_task)
                .await
                .expect("runtime cancellation releases runHeld promptly")
                .unwrap();
        assert!(cancellation_result.is_err());

        let after_cancel_render = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"render",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&after_cancel_render)["handled"], false,
            "a cancelled action fences its shared VM until an explicit remount");
        assert!(!lock(&session_impl.frames)
            .iter()
            .any(|(frame_runtime, frame)| {
                frame_runtime == &runtime_id && tree_contains_text(&frame["tree"], "late props")
            }));

        let client_identity = lock(&host.client_ui.data)
            .instances
            .get(&runtime_id)
            .expect("mounted Client remains tracked after runHeld")
            .key
            .clone();
        let unmounted = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"unmount",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&unmounted)["handled"], true);
        {
            let data = lock(&host.client_ui.data);
            assert!(!data.instances.contains_key(&runtime_id));
            assert!(!data.instance_by_identity.contains_key(&client_identity));
        }

        let repeated_unmount = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"unmount",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&repeated_unmount)["handled"], false);
        assert!(test_value(&repeated_unmount).get("tree").is_none());

        let stale_run_held = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"runHeld",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "handle":held,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&stale_run_held)["handled"], false);
        assert!(test_value(&stale_run_held).get("tree").is_none());

        // A run failure reports through one authoritative Host ui.fault
        // dispatch. The worker keeps a failed snapshot: later operations on
        // that runtime cannot produce a success frame, and a fresh mount is
        // the recovery boundary.
        let failed_run_parent = host
            .dispatch_client_ui_control(
                host_generated_json(render(json!({"failRun":true}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_run_revision = failed_run_parent.render_revision.unwrap();
        let failed_run_mount = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":failed_run_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_run_runtime = test_value(&failed_run_mount)["runtimeId"]
            .as_str()
            .unwrap()
            .to_owned();
        let failed_run_commit = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":failed_run_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&failed_run_commit)["handled"], true);
        let ready_to_fail = wait_for_client_frame(
            &session_impl,
            &failed_run_runtime,
            "run-held setup",
            |frame| tree_contains_text(&frame["tree"], "ready to fail"),
        )
        .await;
        let failed_run_handle = tree_first_press_handle(&ready_to_fail["tree"]).unwrap();
        let run_fault = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"runHeld",
                    "runtimeId":failed_run_runtime,
                    "render_revision":failed_run_revision,
                    "handle":failed_run_handle,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&run_fault)["fault"]["phase"], "run");
        assert_eq!(test_value(&run_fault)["fault"]["source"], "worker");
        assert_eq!(test_value(&run_fault)["outcome"], json!({"status":"unknown","runtime":"unavailable"}));
        let async_run_fault = wait_for_client_frame(
            &session_impl,
            &failed_run_runtime,
            "run fault snapshot",
            |frame| frame.get("fault").is_some(),
        )
        .await;
        assert_eq!(async_run_fault["fault"]["phase"], "run");
        assert_eq!(async_run_fault["fault"]["source"], "worker");
        assert_eq!(async_run_fault["renderRevision"], failed_run_revision);
        assert!(async_run_fault.get("frameSequence").is_none());
        assert!(async_run_fault.get("tree").is_none());
        assert!(
            lock(&host.client_ui.data).instances[&failed_run_runtime].worker_fault_event_dispatched
        );
        let stale_after_run_fault = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"setProps",
                    "runtimeId":failed_run_runtime,
                    "render_revision":failed_run_revision,
                    "props":{"title":"ready to fail","failRun":true},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&stale_after_run_fault)["handled"], false);
        assert!(test_value(&stale_after_run_fault).get("tree").is_none());

        let recovered_parent = host
            .dispatch_client_ui_control(host_generated_json(render(json!({}))), session.as_ref())
            .await
            .unwrap();
        let recovered_revision = recovered_parent.render_revision.unwrap();
        let recovered_mount = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":recovered_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let recovered_runtime = test_value(&recovered_mount)["runtimeId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(recovered_runtime, failed_run_runtime);
        assert!(tree_contains_text(
            &test_value(&recovered_mount)["tree"],
            "initial"
        ));
        let recovered_commit = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":recovered_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&recovered_commit)["handled"], true);

        // Once the callback posts, a concurrent worker unmount can make the
        // final render stale. That is an unknown outcome, never a safe no-op.
        let recovered_posted = wait_for_client_frame(
            &session_impl,
            &recovered_runtime,
            "recovered initial post",
            |frame| tree_contains_text(&frame["tree"], "posted"),
        )
        .await;
        let race_handle = tree_press_handle_for_key(&recovered_posted["tree"], "query").unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let delivered = Arc::new(tokio::sync::Notify::new());
        let race_frame_text = "Input changed (change): accepted before unmount";
        assert!(!lock(&session_impl.frames)
            .iter()
            .any(|(frame_runtime, frame)| {
                frame_runtime == &recovered_runtime
                    && tree_contains_text(&frame["tree"], race_frame_text)
            }));
        *lock(&session_impl.frame_gate) = Some(UiLifecycleFrameGate {
            runtime_id: recovered_runtime.clone(),
            expected_text: race_frame_text,
            entered: entered.clone(),
            release: release.clone(),
            delivered: delivered.clone(),
        });
        let race_operation = host_generated_json(json!({
            "type":"runHeld",
            "runtimeId":recovered_runtime,
            "render_revision":recovered_revision,
            "handle":race_handle,
            "event":{"kind":"change","value":"accepted before unmount"},
        }));
        let race_host = host.clone();
        let race_session = session.clone();
        let race_task = tokio::spawn(async move {
            race_host
                .dispatch_client_ui_operation(race_operation, race_session.as_ref())
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("the accepted callback's unique post reached frame emission");
        assert!(!lock(&session_impl.frames)
            .iter()
            .any(|(frame_runtime, frame)| {
                frame_runtime == &recovered_runtime
                    && tree_contains_text(&frame["tree"], race_frame_text)
            }));
        assert!(!race_task.is_finished());
        let worker_generation = lock(&host.client_ui.data).instances[&recovered_runtime]
            .worker_generation
            .unwrap();
        let worker_unmount = host
            .run_client_worker_operation(host_generated_json(json!({
                "type":"unmount",
                "runtimeId":recovered_runtime,
                "generation":worker_generation,
            })))
            .await
            .map_err(ModError::from)
            .unwrap();
        assert_eq!(test_value(&worker_unmount)["unmounted"], true);
        assert!(
            !race_task.is_finished(),
            "the callback's post remains unsettled until its frame is delivered"
        );
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), delivered.notified())
            .await
            .expect("the accepted callback's frame was delivered after release");
        assert!(lock(&session_impl.frames)
            .iter()
            .any(|(frame_runtime, frame)| {
                frame_runtime == &recovered_runtime
                    && tree_contains_text(&frame["tree"], race_frame_text)
            }));
        let race_error = tokio::time::timeout(Duration::from_secs(2), race_task)
            .await
            .expect("the accepted action resolves after post settlement")
            .unwrap()
            .unwrap_err();
        assert!(race_error
            .to_string()
            .contains("accepted but its follow-up frame is stale"));

        // A Host removal first erases the instance under the data lock and
        // only afterwards cancels its posts. Exercise that gap explicitly:
        // the worker stays mounted, so a follow-up render alone can succeed.
        for race_value in [
            "host-removal-before-frame",
            "host-removal-no-props",
            "host-removal-during-frame",
        ] {
            let parent = host
                .dispatch_client_ui_control(
                    host_generated_json(render(json!({}))),
                    session.as_ref(),
                )
                .await
                .unwrap();
            let revision = parent.render_revision.unwrap();
            let mount = host
                .dispatch_client_ui_operation(
                    host_generated_json(json!({
                        "type":"mount", "surface":"desktop", "component":"Pane",
                        "instance_id":"pane-1", "plugin":"review", "client":"card",
                        "module":client_module, "render_revision":revision,
                        "columns":80, "rows":24,
                    })),
                    session.as_ref(),
                )
                .await
                .unwrap();
            let runtime_id = test_value(&mount)["runtimeId"].as_str().unwrap().to_owned();
            let commit = host
                .dispatch_client_ui_operation(
                    host_generated_json(json!({
                        "type":"draw_commit", "surface":"desktop", "component":"Pane",
                        "instance_id":"pane-1", "render_revision":revision,
                        "clients":[{"plugin":"review","key":"card","module":client_module}],
                    })),
                    session.as_ref(),
                )
                .await
                .unwrap();
            assert_eq!(test_value(&commit)["handled"], true);
            let posted = wait_for_client_frame(&session_impl, &runtime_id, "race setup", |frame| {
                tree_contains_text(&frame["tree"], "posted")
            })
            .await;
            let handle = tree_press_handle_for_key(&posted["tree"], "query").unwrap();
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let completed = Arc::new(tokio::sync::Notify::new());
            if race_value == "host-removal-during-frame" {
                *lock(&session_impl.frame_gate) = Some(UiLifecycleFrameGate {
                    runtime_id: runtime_id.clone(),
                    expected_text: race_value,
                    entered: entered.clone(),
                    release: release.clone(),
                    delivered: completed.clone(),
                });
            } else {
                *lock(&session_impl.model_gate) = Some(UiLifecycleModelGate {
                    entered: entered.clone(),
                    release: release.clone(),
                    completed: completed.clone(),
                });
            }
            let operation = host_generated_json(json!({
                "type":"runHeld", "runtimeId":runtime_id, "render_revision":revision,
                "handle":handle, "event":{"kind":"change","value":race_value},
            }));
            let race_host = host.clone();
            let race_session = session.clone();
            let action = tokio::spawn(async move {
                race_host
                    .dispatch_client_ui_operation(operation, race_session.as_ref())
                    .await
            });
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .expect("the real ui.message hook reached its controlled gate");
            assert!(!action.is_finished(), "the post is still in flight");
            assert!(!lock(&session_impl.frames).iter().any(|(id, frame)| {
                id == &runtime_id && tree_contains_text(&frame["tree"], race_value)
            }));
            let generation = {
                let mut data = lock(&host.client_ui.data);
                let instance = data.instances.get(&runtime_id).unwrap().clone();
                remove_instance_if_current(&mut data, &runtime_id, &instance.key);
                instance.worker_generation.unwrap()
            };
            // Intentionally delay fail_post_actions_for_runtime until after
            // the action returns, matching the production removal window.
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(2), completed.notified())
                .await
                .expect("the gated session API or frame delivery completed");
            let result = tokio::time::timeout(Duration::from_secs(2), action)
                .await
                .expect("the accepted action resolves")
                .unwrap();
            let error = result.expect_err(
                "a post for a removed Host instance must not be acknowledged as delivered",
            );
            let expected_error = if race_value == "host-removal-before-frame" {
                "Client runtime was removed before its frame was delivered"
            } else {
                "Client runtime changed before the post completed"
            };
            assert!(
                error.to_string().contains(expected_error),
                "{race_value}: unexpected error {error}"
            );
            let delivered = lock(&session_impl.frames).iter().any(|(id, frame)| {
                id == &runtime_id && tree_contains_text(&frame["tree"], race_value)
            });
            assert_eq!(delivered, race_value == "host-removal-during-frame");
            host.client_ui
                .fail_post_actions_for_runtime(&runtime_id, "test removal cleanup");
            let cleanup = host
                .run_client_worker_operation(host_generated_json(json!({
                    "type":"unmount", "runtimeId":runtime_id, "generation":generation,
                })))
                .await
                .map_err(ModError::from)
                .unwrap();
            assert_eq!(test_value(&cleanup)["unmounted"], true);
        }

        // Timer failures have no operation result. The Host emits a local
        // fault snapshot immediately, even though this plugin has no
        // ui.fault listener to trigger an invalidation.
        let timer_parent = host
            .dispatch_client_ui_control(
                host_generated_json(render(json!({"timerFault":true}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let timer_revision = timer_parent.render_revision.unwrap();
        let timer_mount = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":timer_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let timer_runtime = test_value(&timer_mount)["runtimeId"]
            .as_str()
            .unwrap()
            .to_owned();
        let timer_commit = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":timer_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&timer_commit)["handled"], true);
        let timer_fault = wait_for_client_frame(
            &session_impl,
            &timer_runtime,
            "timer fault snapshot",
            |frame| frame.get("fault").is_some(),
        )
        .await;
        assert_eq!(timer_fault["fault"]["phase"], "run");
        assert_eq!(timer_fault["fault"]["source"], "worker");
        assert_eq!(timer_fault["renderRevision"], timer_revision);
        assert!(timer_fault.get("frameSequence").is_none());
        assert!(lock(&host.client_ui.data).instances[&timer_runtime].worker_fault_event_dispatched);
        {
            let invalidations = lock(&session_impl.invalidations);
            assert_eq!(
                invalidations.len(),
                1,
                "the successful same-plugin reload invalidates the already-rendered site once"
            );
            let event = &invalidations[0];
            assert_eq!(event["session_id"], "ui-lifecycle-session");
            assert!(event["uuid"].as_str().unwrap().starts_with("mod-ui-state-"));
            assert_eq!(
                event["instances"],
                json!([{
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                }]),
                "the reload invalidation is scoped to the rendered parent site"
            );
        }
        let timer_stale = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"resize",
                    "runtimeId":timer_runtime,
                    "render_revision":timer_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&timer_stale)["handled"], false);
        assert!(test_value(&timer_stale).get("tree").is_none());
        let timer_generation = lock(&host.client_ui.data).instances[&timer_runtime]
            .worker_generation
            .unwrap();
        let duplicate_timer_fault = json!({
            "kind":"ui.client.event",
            "runtimeId":timer_runtime,
            "generation":timer_generation,
            "plugin":"review",
            "element":"card",
            "module":client_module,
            "action":"fault",
            "phase":"run",
            "reason":"client timer exploded",
        });
        let duplicate_timer_fault =
            Utf16JsonProjection::from_serde_value(&duplicate_timer_fault).unwrap();
        host.handle_client_ui_background_event(&duplicate_timer_fault, session.as_ref())
            .await
            .unwrap();
        assert_eq!(
            lock(&session_impl.frames)
                .iter()
                .filter(|(runtime, frame)| {
                    runtime == &timer_runtime && frame.get("fault").is_some()
                })
                .count(),
            1,
            "a repeated worker fault does not emit a second local failure snapshot"
        );

        // Mount-time render failure uses the same one-shot fault path and
        // removes the incomplete runtime mapping before returning its union.
        let failed_parent = host
            .dispatch_client_ui_control(
                host_generated_json(render(json!({"fail":true}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_revision = failed_parent.render_revision.unwrap();
        let failed_mount = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":failed_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_mount_runtime = test_value(&failed_mount)["runtimeId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(test_value(&failed_mount)["handled"], false);
        assert_eq!(test_value(&failed_mount)["fault"]["phase"], "render");
        assert_eq!(test_value(&failed_mount)["fault"]["source"], "worker");
        assert!(!lock(&session_impl.frames).iter().any(|(runtime, frame)| {
            runtime == &failed_mount_runtime && frame.get("fault").is_some()
        }));
        assert!(!lock(&host.client_ui.data)
            .instances
            .contains_key(&failed_mount_runtime));
        let after_mount_fault = host.ui_render_generation();
        let stale_after_mount_fault = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"render",
                    "runtimeId":failed_mount_runtime,
                    "render_revision":failed_revision,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&stale_after_mount_fault)["handled"], false);
        assert_eq!(host.ui_render_generation(), after_mount_fault);

        let committed_failure = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":failed_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(test_value(&committed_failure)["handled"], true);

        let unload_invalidation_start = lock(&session_impl.invalidations).len();
        host.unload("review@user").await.unwrap();
        let invalidations = lock(&session_impl.invalidations);
        let unload_invalidations = &invalidations[unload_invalidation_start..];
        assert_eq!(
            unload_invalidations.len(),
            2,
            "Native's unload path invalidates at removal start and after table resolution"
        );
        for event in unload_invalidations {
            assert_eq!(event["session_id"], "ui-lifecycle-session");
            assert!(event["uuid"].as_str().unwrap().starts_with("mod-ui-state-"));
            assert_eq!(
                event["instances"],
                json!([{
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                }]),
                "each unload invalidation targets only the tracked parent site"
            );
        }
        assert_ne!(
            unload_invalidations[0]["uuid"],
            unload_invalidations[1]["uuid"]
        );
        assert!(host.client_ui.data.lock().unwrap().instances.is_empty());
        let stale_after_unload = host
            .dispatch_client_ui_control(
                client_press("submit", json!({"type":"press"})),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            test_value(&stale_after_unload.response)["handled"],
            false,
            "a Client control from an unloaded draw is stale"
        );
    }

    #[tokio::test]
    async fn ui_client_module_errors_when_plugin_has_no_surface_modules() {
        let host = ModHost::start(None).await.unwrap();
        let session = UiLifecycleSession::default();
        host.client_ui
            .install_loaded_plugin("surface-less", "surface-less@user", None);

        for plugin in ["unavailable-plugin", "surface-less"] {
            let result = host
                .dispatch_client_ui_control(
                    host_generated_json(json!({
                        "subtype": "ui_client_module",
                        "plugin": plugin,
                    })),
                    &session,
                )
                .await;
            match result {
                Err(ModError::Hook(message)) => assert_eq!(
                    message,
                    format!(
                        "ui_client_module: plugin {plugin} is not loaded or its hooks module names no surface module"
                    )
                ),
                other => panic!("expected an unavailable surface-module error, got {other:?}"),
            }
        }
    }

    #[test]
    fn stale_mount_cleanup_does_not_remove_a_newer_runtime_mapping() {
        let site = ClientUiRenderSite::new(ClientUiComponent::Pane, "pane-1").unwrap();
        let identity = ClientUiIdentity::new(
            ExactHookText::from_text("plugin"),
            ExactHookText::from_text("view.ts"),
            "card".encode_utf16().collect(),
        )
        .unwrap();
        let key = ClientUiInstanceKey { site, identity };
        let mut data = ClientUiHostData::default();
        data.instance_by_identity
            .insert(key.clone(), "new-runtime".into());

        remove_instance_if_current(&mut data, "old-runtime", &key);

        assert_eq!(
            data.instance_by_identity.get(&key).map(String::as_str),
            Some("new-runtime")
        );
    }

    #[test]
    fn plugin_unload_returns_sites_only_when_the_last_storage_is_removed() {
        let state = ClientUiHostState::default();
        state.install_loaded_plugin("plugin", "plugin@user", None);
        state.install_loaded_plugin("plugin", "plugin@project", None);
        let site = ClientUiRenderSite::new(ClientUiComponent::Pane, "pane-1").unwrap();
        let identity = ClientUiIdentity::new(
            ExactHookText::from_text("plugin"),
            ExactHookText::from_text("view.ts"),
            "card".encode_utf16().collect(),
        )
        .unwrap();
        lock(&state.data).parents.insert(
            site.clone(),
            ParentRender {
                revision: 1,
                token: None,
                committed_generation: None,
                clients: HashMap::from([(identity, host_generated_json(json!({})))]),
            },
        );

        assert!(state.unload_storage("plugin@user").is_empty());
        let affected = state.unload_storage("plugin@project");
        assert_eq!(affected, vec![site]);
    }

    #[tokio::test]
    async fn stale_draw_unmount_preserves_a_newer_in_flight_parent_render() {
        let host = ModHost::start(None).await.unwrap();
        let session = UiLifecycleSession::default();
        let site = ClientUiRenderSite::new(ClientUiComponent::Pane, "pane-1").unwrap();
        let new_token = host.client_ui.fault_registry.begin_render(site.clone(), 2);
        {
            let mut data = lock(&host.client_ui.data);
            data.parents.insert(
                site.clone(),
                ParentRender {
                    revision: 1,
                    token: None,
                    committed_generation: None,
                    clients: HashMap::new(),
                },
            );
            data.latest_started.insert(site.clone(), 2);
            data.pending_tokens.insert((site.clone(), 2), new_token);
        }

        let result = host
            .dispatch_client_ui_operation(
                host_generated_json(json!({
                    "type":"draw_unmount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":1,
                })),
                &session,
            )
            .await
            .unwrap();

        assert_eq!(test_value(&result)["handled"], true);
        let data = lock(&host.client_ui.data);
        assert_eq!(data.latest_started.get(&site), Some(&2));
        assert!(data.pending_tokens.contains_key(&(site.clone(), 2)));
        assert!(!data.parents.contains_key(&site));
    }

    #[tokio::test]
    async fn parent_press_preflight_uses_worker_url_and_native_file_path_rules() {
        let host = ModHost::start(None).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();

        assert!(host
            .preflight_parent_press_href(
                "www.Example.com:443/a/../b",
                &["http://www.example.com:443/b".into()],
                cwd,
            )
            .await
            .unwrap());
        assert!(!host
            .preflight_parent_press_href(
                "https://example.com/a",
                &["https://example.com/b".into()],
                cwd,
            )
            .await
            .unwrap());
        assert!(host
            .preflight_parent_press_href("/tmp/a.md", &["file:///tmp/a.md".into()], cwd,)
            .await
            .unwrap());
    }

    #[test]
    fn parent_action_handle_accepts_only_positive_safe_integers() {
        assert_eq!(parent_action_handle(Some(&json!(1))), Some(1));
        assert_eq!(parent_action_handle(Some(&json!(0))), None);
        assert_eq!(parent_action_handle(Some(&json!(-1))), None);
        assert_eq!(parent_action_handle(Some(&json!(1.5))), None);
        assert_eq!(
            parent_action_handle(Some(&json!(MAX_JS_SAFE_INTEGER + 1))),
            None
        );
    }
}
