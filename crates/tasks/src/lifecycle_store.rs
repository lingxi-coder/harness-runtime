//! Mutation-boundary SDK task events (2.1.263 `Mlo` / `Rlo`).
//!
//! Snapshot only fields visible in the SDK, never task output, transcripts,
//! credentials or process handles. Publishing in the write guard's Drop also
//! covers handler status sinks and early-return paths without polling races.
use crate::state::TaskState;
use serde_json::{json, Map, Value};
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
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Value>>>,
}

#[derive(Clone, PartialEq)]
struct Projection {
    started: Map<String, Value>,
    patch: Map<String, Value>,
}

fn project(state: &TaskState, include_started: bool) -> Option<Projection> {
    if matches!(state, TaskState::LocalAgent(agent) if agent.is_observer) {
        return None;
    }
    let base = state.base();
    let mut started = Map::new();
    started.insert("type".into(), json!("system"));
    started.insert("subtype".into(), json!("task_started"));
    started.insert("task_id".into(), json!(base.id));
    started.insert("description".into(), json!(base.description));
    started.insert(
        "task_type".into(),
        json!(crate::handle::task_type_to_wire(base.task_type)),
    );
    if let Some(id) = &base.tool_use_id {
        started.insert("tool_use_id".into(), json!(id));
    }
    let mut patch = Map::new();
    patch.insert(
        "status".into(),
        json!(crate::handle::status_to_wire(base.status)),
    );
    patch.insert("description".into(), json!(base.description));
    patch.insert("total_paused_ms".into(), json!(base.total_paused_ms));
    if let Some(time) = base
        .end_time
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    {
        patch.insert("end_time".into(), json!(time.as_millis() as u64));
    }
    match state {
        TaskState::LocalBash(shell) => {
            if base.creator_agent_id.is_some() {
                started.insert("owned_by_subagent".into(), json!(true));
            }
            if let Some(backgrounded) = shell.is_backgrounded {
                patch.insert("is_backgrounded".into(), json!(backgrounded));
            }
        }
        TaskState::LocalAgent(agent) => {
            started.insert("subagent_type".into(), json!(agent.subagent_type));
            if include_started {
                started.insert("prompt".into(), json!(agent.prompt));
            }
            patch.insert("is_backgrounded".into(), json!(agent.is_backgrounded));
        }
        TaskState::LocalWorkflow(workflow) => {
            started.insert("workflow_name".into(), json!(workflow.workflow_id));
        }
        _ => {}
    }
    let error = match state {
        TaskState::LocalAgent(agent) => agent.error.as_ref(),
        TaskState::LocalWorkflow(workflow) => workflow.outcome.error.as_ref(),
        TaskState::LocalFusion(fusion) => fusion.error.as_ref(),
        _ => None,
    };
    if let Some(error) = error {
        patch.insert("error".into(), json!(error));
    }
    if matches!(state, TaskState::AutoModeScan(_)) {
        started.insert("skip_transcript".into(), json!(true));
        started.insert("ambient".into(), json!(true));
    }
    if let Some(backgrounded) = patch.get("is_backgrounded") {
        started.insert("is_backgrounded".into(), backgrounded.clone());
    }
    Some(Projection { started, patch })
}

/// Rlo's terminal receipt is SDK output, independent of the model notification
/// queue and its `notified` bit. A resume can produce another natural edge.
fn natural_terminal_receipt(state: &TaskState, status: &str) -> Value {
    let base = state.base();
    // uln passes the final text (or description fallback); MRe passes the
    // failure text. Other state variants have no stored terminal summary.
    let summary = match state {
        TaskState::LocalAgent(agent) if status == "completed" => agent
            .outcome
            .result
            .as_deref()
            .filter(|text| !text.is_empty()),
        TaskState::LocalAgent(agent) if status == "failed" => agent.error.as_deref(),
        _ => None,
    }
    .unwrap_or(&base.description);
    let mut event = json!({
        "type": "system", "subtype": "task_notification", "task_id": base.id,
        "status": status, "summary": summary,
        "output_file": base.output_file.to_string_lossy(),
    });
    if let Some(tool_use_id) = &base.tool_use_id {
        event["tool_use_id"] = json!(tool_use_id);
    }
    let usage = match state {
        TaskState::LocalAgent(agent) if status == "completed" => agent.outcome.usage.as_ref(),
        TaskState::LocalFusion(fusion) => fusion.usage.as_ref(),
        _ => None,
    };
    if let Some(usage) = usage {
        event["usage"] = json!({"total_tokens": usage.subagent_tokens, "tool_uses": usage.tool_uses, "duration_ms": usage.duration_ms});
    } else if let TaskState::LocalWorkflow(workflow) = state {
        if workflow.outcome.progress_counts_available {
            event["usage"] = json!({"total_tokens": workflow.outcome.total_tokens, "tool_uses": workflow.outcome.total_tool_calls, "duration_ms": workflow.outcome.duration_ms});
        }
    }
    if matches!(state, TaskState::AutoModeScan(_)) {
        event["skip_transcript"] = json!(true);
        event["ambient"] = json!(true);
    }
    event
}

impl TaskRows {
    /// Emit an explicitly claimed SDK event after its task mutation is committed.
    pub(crate) fn emit(&self, event: Value) {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|sender| sender.send(event.clone()).is_ok());
    }

    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<Value> {
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
        self.emit(json!({"type":"task_feature", "feature_name":name, "error_code":error}));
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
    subscribers: &'a Mutex<Vec<mpsc::UnboundedSender<Value>>>,
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
                None => Value::Object(after.started),
                Some(previous) => {
                    let mut patch = Map::new();
                    for (key, value) in after.patch {
                        if previous.patch.get(&key) != Some(&value) {
                            patch.insert(key, value);
                        }
                    }
                    // Undefined error/background fields are omitted by Rlo;
                    // clearing end_time is represented as null on JSON wire.
                    if previous.patch.contains_key("end_time") && state.base().end_time.is_none() {
                        patch.insert("end_time".into(), Value::Null);
                    }
                    if patch.is_empty() {
                        continue;
                    }
                    json!({"type": "system", "subtype": "task_updated", "task_id": id, "patch": patch})
                }
            };
            for sender in senders.iter() {
                let _ = sender.send(event.clone());
            }
            let new_status = crate::handle::status_to_wire(state.base().status);
            let old_status = before
                .get(id)
                .and_then(|row| row.patch.get("status"))
                .and_then(Value::as_str);
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
                    let counter =
                        json!({"type":"task_feature", "feature_name":feature, "error_code":error});
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
