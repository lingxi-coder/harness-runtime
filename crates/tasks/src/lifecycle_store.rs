//! Mutation-boundary SDK task events (2.1.263 `Mlo` / `Rlo`).
//!
//! Snapshot only fields visible in the SDK, never task output, transcripts,
//! credentials or process handles. Publishing in the write guard's Drop also
//! covers handler status sinks and early-return paths without polling races.
use crate::state::TaskState;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::sync::Mutex;
use tokio::sync::{mpsc, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Default)]
pub(crate) struct TaskRows {
    rows: RwLock<HashMap<String, TaskState>>,
    /// JavaScript object property order is observable by `$.agent.list`'s
    /// duplicate reducer. Keep insertion order separately from the hash table.
    order: Mutex<Vec<String>>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Utf16JsonProjection>>>,
}

#[derive(Clone, PartialEq)]
struct Projection {
    started: Utf16JsonProjection,
    patch: Vec<(String, Utf16JsonProjection)>,
}

fn generated(value: Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value)
        .expect("generated task lifecycle value is JSON representable")
}

fn source_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("generated task lifecycle object is valid")
}

/// Exact source description for model-visible task lifecycle rows. The base
/// description is retained only as a display fallback for task kinds that do
/// not keep an independent source carrier.
fn source_description(state: &TaskState) -> Utf16JsonProjection {
    match state {
        TaskState::Monitor(monitor) => monitor.source_description.clone(),
        TaskState::LocalAgent(agent) => agent
            .spawned_description
            .clone()
            .unwrap_or_else(|| state.base().description.clone()),
        TaskState::InProcessTeammate(agent) => agent
            .spawned_description
            .clone()
            .unwrap_or_else(|| state.base().description.clone()),
        _ => state.base().description.clone(),
    }
}

fn project(state: &TaskState, include_started: bool) -> Option<Projection> {
    if matches!(state, TaskState::LocalAgent(agent) if agent.is_observer) {
        return None;
    }
    let base = state.base();
    let mut started = vec![
        ("type".into(), Utf16JsonProjection::from("system")),
        ("subtype".into(), Utf16JsonProjection::from("task_started")),
        (
            "task_id".into(),
            Utf16JsonProjection::from(base.id.as_str()),
        ),
        ("description".into(), source_description(state)),
        (
            "task_type".into(),
            Utf16JsonProjection::from(crate::handle::task_type_to_wire(base.task_type)),
        ),
    ];
    if let Some(id) = &base.tool_use_id {
        started.push(("tool_use_id".into(), Utf16JsonProjection::from(id.as_str())));
    }
    let mut patch = vec![
        (
            "status".into(),
            Utf16JsonProjection::from(crate::handle::status_to_wire(base.status)),
        ),
        ("description".into(), source_description(state)),
        (
            "total_paused_ms".into(),
            generated(json!(base.total_paused_ms)),
        ),
    ];
    if let Some(time) = base
        .end_time
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    {
        patch.push(("end_time".into(), generated(json!(time.as_millis() as u64))));
    }
    match state {
        TaskState::LocalBash(shell) => {
            if base.creator_agent_id.is_some() {
                started.push(("owned_by_subagent".into(), generated(json!(true))));
            }
            if let Some(backgrounded) = shell.is_backgrounded {
                patch.push(("is_backgrounded".into(), generated(json!(backgrounded))));
            }
        }
        TaskState::LocalAgent(agent) => {
            started.push((
                "subagent_type".into(),
                Utf16JsonProjection::from(agent.subagent_type.as_str()),
            ));
            if include_started {
                started.push(("prompt".into(), agent.prompt.clone()));
            }
            patch.push((
                "is_backgrounded".into(),
                generated(json!(agent.is_backgrounded)),
            ));
        }
        TaskState::LocalWorkflow(workflow) => started.push((
            "workflow_name".into(),
            Utf16JsonProjection::from(workflow.workflow_id.as_str()),
        )),
        _ => {}
    }
    let error = match state {
        TaskState::LocalAgent(agent) => agent.error.as_ref(),
        TaskState::LocalWorkflow(workflow) => workflow.outcome.error.as_ref(),
        TaskState::LocalFusion(fusion) => fusion.error.as_ref(),
        _ => None,
    };
    if let Some(error) = error {
        patch.push(("error".into(), Utf16JsonProjection::from(error.as_str())));
    }
    if let Some((_, backgrounded)) = patch.iter().find(|(key, _)| key == "is_backgrounded") {
        started.push(("is_backgrounded".into(), backgrounded.clone()));
    }
    Some(Projection {
        started: source_object(started),
        patch,
    })
}

/// Rlo's terminal receipt is SDK output, independent of the model notification
/// queue and its `notified` bit. A resume can produce another natural edge.
fn natural_terminal_receipt(state: &TaskState, status: &str) -> Utf16JsonProjection {
    let base = state.base();
    let summary = match state {
        TaskState::LocalAgent(agent) if status == "completed" => agent
            .outcome
            .result
            .as_ref()
            .filter(|result| {
                result
                    .string_units("")
                    .is_some_and(|units| !units.is_empty())
            })
            .cloned(),
        TaskState::LocalAgent(agent) if status == "failed" => {
            agent.error.as_deref().map(Utf16JsonProjection::from)
        }
        _ => None,
    }
    .unwrap_or_else(|| source_description(state));
    let mut fields = vec![
        ("type".into(), Utf16JsonProjection::from("system")),
        (
            "subtype".into(),
            Utf16JsonProjection::from("task_notification"),
        ),
        (
            "task_id".into(),
            Utf16JsonProjection::from(base.id.as_str()),
        ),
        ("status".into(), Utf16JsonProjection::from(status)),
        ("summary".into(), summary),
        (
            "output_file".into(),
            Utf16JsonProjection::from(base.output_file.to_string_lossy().as_ref()),
        ),
    ];
    if let Some(tool_use_id) = &base.tool_use_id {
        fields.push((
            "tool_use_id".into(),
            Utf16JsonProjection::from(tool_use_id.as_str()),
        ));
    }
    let usage = match state {
        TaskState::LocalAgent(agent) if status == "completed" => agent.outcome.usage.as_ref(),
        TaskState::LocalFusion(fusion) => fusion.usage.as_ref(),
        _ => None,
    };
    if let Some(usage) = usage {
        fields.push((
            "usage".into(),
            generated(json!({
                "total_tokens": usage.subagent_tokens,
                "tool_uses": usage.tool_uses,
                "duration_ms": usage.duration_ms,
            })),
        ));
    } else if let TaskState::LocalWorkflow(workflow) = state {
        if workflow.outcome.progress_counts_available {
            fields.push((
                "usage".into(),
                generated(json!({
                    "total_tokens": workflow.outcome.total_tokens,
                    "tool_uses": workflow.outcome.total_tool_calls,
                    "duration_ms": workflow.outcome.duration_ms,
                })),
            ));
        }
    }
    if matches!(state, TaskState::AutoModeScan(_)) {
        fields.push(("skip_transcript".into(), generated(json!(true))));
        fields.push(("ambient".into(), generated(json!(true))));
    }
    source_object(fields)
}

impl TaskRows {
    /// Emit an explicitly claimed SDK event after its task mutation is committed.
    pub(crate) fn emit(&self, event: Utf16JsonProjection) {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|sender| sender.send(event.clone()).is_ok());
    }

    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<Utf16JsonProjection> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sender);
        receiver
    }

    pub(crate) fn try_read(
        &self,
    ) -> Result<RwLockReadGuard<'_, HashMap<String, TaskState>>, tokio::sync::TryLockError> {
        self.rows.try_read()
    }

    pub(crate) fn feature(&self, name: &'static str, error: Option<&'static str>) {
        self.emit(source_object(vec![
            ("type".into(), Utf16JsonProjection::from("task_feature")),
            ("feature_name".into(), Utf16JsonProjection::from(name)),
            ("error_code".into(), generated(json!(error))),
        ]));
    }

    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, HashMap<String, TaskState>> {
        self.rows.read().await
    }

    pub(crate) async fn ordered_states(&self) -> Vec<TaskState> {
        let rows = self.rows.read().await;
        let order = self
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        order
            .iter()
            .filter_map(|id| rows.get(id).cloned())
            .collect()
    }

    pub(crate) async fn write(&self) -> TaskWriteGuard<'_> {
        let rows = self.rows.write().await;
        let listening = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|sender| !sender.is_closed());
        let before = listening.then(|| {
            rows.iter()
                .filter_map(|(id, state)| {
                    project(state, false).map(|projection| (id.clone(), projection))
                })
                .collect()
        });
        TaskWriteGuard {
            rows,
            order: &self.order,
            before,
            subscribers: &self.subscribers,
        }
    }
}

pub(crate) struct TaskWriteGuard<'a> {
    rows: RwLockWriteGuard<'a, HashMap<String, TaskState>>,
    order: &'a Mutex<Vec<String>>,
    before: Option<HashMap<String, Projection>>,
    subscribers: &'a Mutex<Vec<mpsc::UnboundedSender<Utf16JsonProjection>>>,
}
impl Deref for TaskWriteGuard<'_> {
    type Target = HashMap<String, TaskState>;
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}
impl DerefMut for TaskWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rows
    }
}
impl TaskWriteGuard<'_> {
    /// Insert a task row while preserving first-insertion order. Replacing an
    /// existing key does not move its property, matching a JavaScript object.
    pub(crate) fn insert<I>(&mut self, id: I, state: TaskState) -> Option<TaskState>
    where
        I: Into<String>,
    {
        let id = id.into();
        let existed = self.rows.contains_key(&id);
        let previous = self.rows.insert(id.clone(), state);
        if !existed {
            self.order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(id);
        }
        previous
    }

    /// Remove the row and its order entry. A later insert of this ID is a new
    /// property and therefore appears at the end of the registry snapshot.
    pub(crate) fn remove<Q>(&mut self, id: &Q) -> Option<TaskState>
    where
        String: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let removed = self.rows.remove(id);
        if removed.is_some() {
            self.order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|candidate| candidate.borrow() != id);
        }
        removed
    }

    /// Retain rows while pruning the matching ordered-key ledger.
    pub(crate) fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&String, &mut TaskState) -> bool,
    {
        self.rows.retain(|id, state| keep(id, state));
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|id| self.rows.contains_key(id));
    }

    /// Emit a logical mutation boundary while retaining the row lock. This is
    /// used when a shell's start and already-received exit publish atomically.
    pub(crate) fn checkpoint(&mut self) {
        self.emit_changes();
        if self.before.is_some() {
            self.before = Some(
                self.rows
                    .iter()
                    .filter_map(|(id, state)| {
                        project(state, false).map(|projection| (id.clone(), projection))
                    })
                    .collect(),
            );
        }
    }

    fn emit_changes(&self) {
        let Some(before) = &self.before else {
            return;
        };
        let mut senders = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        senders.retain(|sender| !sender.is_closed());
        for (id, state) in self.rows.iter() {
            let Some(after) = project(state, !before.contains_key(id)) else {
                continue;
            };
            let event = match before.get(id) {
                None => after.started,
                Some(previous) => {
                    let mut patch = after
                        .patch
                        .into_iter()
                        .filter(|(key, value)| {
                            previous
                                .patch
                                .iter()
                                .find(|(previous_key, _)| previous_key == key)
                                .is_none_or(|(_, previous_value)| previous_value != value)
                        })
                        .collect::<Vec<_>>();
                    // Undefined error/background fields are omitted by Rlo;
                    // clearing end_time is represented as null on JSON wire.
                    if previous.patch.iter().any(|(key, _)| key == "end_time")
                        && state.base().end_time.is_none()
                    {
                        patch.push(("end_time".into(), generated(Value::Null)));
                    }
                    if patch.is_empty() {
                        continue;
                    }
                    source_object(vec![
                        ("type".into(), Utf16JsonProjection::from("system")),
                        ("subtype".into(), Utf16JsonProjection::from("task_updated")),
                        ("task_id".into(), Utf16JsonProjection::from(id.as_str())),
                        ("patch".into(), source_object(patch)),
                    ])
                }
            };
            for sender in senders.iter() {
                let _ = sender.send(event.clone());
            }
            let new_status = crate::handle::status_to_wire(state.base().status);
            let old_status = before.get(id).and_then(|row| {
                row.patch
                    .iter()
                    .find(|(key, _)| key == "status")
                    .and_then(|(_, value)| value.string_at(""))
            });
            if old_status.is_some_and(|status| !matches!(status, "completed" | "failed" | "killed"))
                && matches!(new_status, "completed" | "failed")
            {
                let receipt = natural_terminal_receipt(state, new_status);
                for sender in senders.iter() {
                    let _ = sender.send(receipt.clone());
                }
                let feature = match state {
                    TaskState::LocalBash(_) => Some("task_local_shell"),
                    TaskState::LocalAgent(agent) if agent.subagent_type == "main-session" => {
                        Some("task_main_session")
                    }
                    TaskState::LocalAgent(_) => Some("task_local_agent"),
                    TaskState::RemoteAgent(_) => Some("task_remote_agent"),
                    TaskState::Dream(_) => Some("task_dream"),
                    _ => None,
                };
                if let Some(feature) = feature {
                    let error = (new_status == "failed").then(|| format!("{feature}_failed"));
                    let counter = source_object(vec![
                        ("type".into(), Utf16JsonProjection::from("task_feature")),
                        ("feature_name".into(), Utf16JsonProjection::from(feature)),
                        ("error_code".into(), generated(json!(error))),
                    ]);
                    for sender in senders.iter() {
                        let _ = sender.send(counter.clone());
                    }
                }
            }
        }
    }
}

impl Drop for TaskWriteGuard<'_> {
    fn drop(&mut self) {
        self.emit_changes();
    }
}
