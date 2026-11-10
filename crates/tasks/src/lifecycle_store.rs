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
use std::ops::Deref;
use std::sync::Mutex;
use tokio::sync::{mpsc, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Default)]
pub(crate) struct TaskRows {
    rows: RwLock<HashMap<String, TaskState>>,
    /// JavaScript object property order is observable by `$.agent.list`'s
    /// duplicate reducer. Keep insertion order separately from the hash table.
    order: Mutex<Vec<String>>,
    revisions: Mutex<RowRevisions>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Utf16JsonProjection>>>,
}

#[derive(Default)]
struct RowRevisions {
    next: u64,
    next_ordinal: u64,
    rows: HashMap<String, RowRevision>,
}

struct RowRevision {
    revision: u64,
    ordinal: u64,
}

#[derive(Clone, PartialEq)]
struct Projection {
    started: Option<Utf16JsonProjection>,
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
    let mut started = include_started.then(|| {
        vec![
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
        ]
    });
    if let (Some(started), Some(id)) = (&mut started, &base.tool_use_id) {
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
                if let Some(started) = &mut started {
                    started.push(("owned_by_subagent".into(), generated(json!(true))));
                }
            }
            if let Some(backgrounded) = shell.is_backgrounded {
                patch.push(("is_backgrounded".into(), generated(json!(backgrounded))));
            }
        }
        TaskState::LocalAgent(agent) => {
            if let Some(started) = &mut started {
                started.push((
                    "subagent_type".into(),
                    Utf16JsonProjection::from(agent.subagent_type.as_str()),
                ));
                started.push(("prompt".into(), agent.prompt.clone()));
            }
            patch.push((
                "is_backgrounded".into(),
                generated(json!(agent.is_backgrounded)),
            ));
        }
        TaskState::LocalWorkflow(workflow) => {
            if let Some(started) = &mut started {
                started.push((
                    "workflow_name".into(),
                    Utf16JsonProjection::from(workflow.workflow_id.as_str()),
                ));
            }
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
        patch.push(("error".into(), Utf16JsonProjection::from(error.as_str())));
    }
    if let Some(started) = &mut started {
        if let Some((_, backgrounded)) = patch.iter().find(|(key, _)| key == "is_backgrounded") {
            started.push(("is_backgrounded".into(), backgrounded.clone()));
        }
    }
    Some(Projection {
        started: started.map(source_object),
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

    /// Borrow rows in their observable insertion order and return only the
    /// selected projection. Revisions change before any mutable row access,
    /// including the conservative mutable-iterator fallback.
    pub(crate) async fn ordered_find_map<R>(
        &self,
        mut select: impl FnMut(&TaskState, u64) -> Option<R>,
    ) -> Option<R> {
        let rows = self.rows.read().await;
        let order = self
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revisions = self
            .revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        order.iter().find_map(|id| {
            rows.get(id).and_then(|state| {
                select(state, revisions.rows.get(id).map_or(0, |row| row.revision))
            })
        })
    }

    pub(crate) async fn write(&self) -> TaskWriteGuard<'_> {
        let rows = self.rows.write().await;
        let listening = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|sender| !sender.is_closed());
        TaskWriteGuard {
            rows,
            order: &self.order,
            revisions: &self.revisions,
            before: listening.then(HashMap::new),
            subscribers: &self.subscribers,
        }
    }
}

pub(crate) struct TaskWriteGuard<'a> {
    rows: RwLockWriteGuard<'a, HashMap<String, TaskState>>,
    order: &'a Mutex<Vec<String>>,
    revisions: &'a Mutex<RowRevisions>,
    /// Only touched keys are captured. `None` records an absent/hidden row,
    /// which must still produce a start event if it becomes publicly visible.
    before: Option<HashMap<String, Option<Projection>>>,
    subscribers: &'a Mutex<Vec<mpsc::UnboundedSender<Utf16JsonProjection>>>,
}
impl Deref for TaskWriteGuard<'_> {
    type Target = HashMap<String, TaskState>;
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}
impl TaskWriteGuard<'_> {
    fn capture(&mut self, id: &str) {
        if let Some(before) = &mut self.before {
            before
                .entry(id.to_owned())
                .or_insert_with(|| self.rows.get(id).and_then(|state| project(state, false)));
        }
    }

    fn revise(&self, id: &str) {
        let mut revisions = self
            .revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revisions.next = revisions.next.wrapping_add(1);
        let revision = revisions.next;
        if let Some(row) = revisions.rows.get_mut(id) {
            row.revision = revision;
        } else {
            let ordinal = revisions.next_ordinal;
            revisions.next_ordinal = revisions.next_ordinal.wrapping_add(1);
            revisions
                .rows
                .insert(id.to_owned(), RowRevision { revision, ordinal });
        }
    }

    fn capture_all(&mut self) {
        let ids = self.rows.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.capture(&id);
            self.revise(&id);
        }
    }

    /// A mutable iterator can change any yielded row. Unlike keyed access it
    /// must capture the complete possible mutation set before yielding.
    pub(crate) fn values_mut(
        &mut self,
    ) -> std::collections::hash_map::ValuesMut<'_, String, TaskState> {
        self.capture_all();
        self.rows.values_mut()
    }

    pub(crate) fn get_mut<Q>(&mut self, id: &Q) -> Option<&mut TaskState>
    where
        String: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let key = self.rows.get_key_value(id)?.0.clone();
        self.capture(&key);
        self.revise(&key);
        self.rows.get_mut(id)
    }

    /// Transcript writes cannot affect SDK lifecycle output. Keep the accessor
    /// restricted to messages so callers cannot silently mutate public fields.
    pub(crate) fn agent_transcript_mut(
        &mut self,
        id: &str,
    ) -> Option<&mut Vec<lingxi_core::types::ConversationMessage>> {
        if !matches!(self.rows.get(id), Some(TaskState::LocalAgent(_))) {
            return None;
        }
        self.revise(id);
        match self.rows.get_mut(id) {
            Some(TaskState::LocalAgent(agent)) => Some(&mut agent.messages),
            _ => None,
        }
    }

    /// These local list facts are deliberately absent from the SDK projection
    /// and from direct Agent transcript output.
    pub(crate) fn agent_local_facts_mut(
        &mut self,
        agent_id: lingxi_core::types::AgentId,
    ) -> Option<&mut lingxi_core::host::task_registry::AgentListLocalLifecycleFacts> {
        self.rows.values_mut().find_map(|state| match state {
            TaskState::LocalAgent(agent) if agent.agent_id == agent_id => {
                Some(&mut agent.agent_list_lifecycle)
            }
            _ => None,
        })
    }

    /// Insert a task row while preserving first-insertion order. Replacing an
    /// existing key does not move its property, matching a JavaScript object.
    pub(crate) fn insert<I>(&mut self, id: I, state: TaskState) -> Option<TaskState>
    where
        I: Into<String>,
    {
        let id = id.into();
        self.capture(&id);
        self.revise(&id);
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
        let key = self.rows.get_key_value(id)?.0.clone();
        self.capture(&key);
        let removed = self.rows.remove(id);
        if removed.is_some() {
            let mut revisions = self
                .revisions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            revisions.rows.remove::<str>(key.as_str());
            drop(revisions);
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
        self.capture_all();
        self.rows.retain(|id, state| keep(id, state));
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|id| self.rows.contains_key(id));
        let mut revisions = self
            .revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revisions.rows.retain(|id, _| self.rows.contains_key(id));
    }

    /// Emit a logical mutation boundary while retaining the row lock. This is
    /// used when a shell's start and already-received exit publish atomically.
    pub(crate) fn checkpoint(&mut self) {
        self.emit_changes();
        if let Some(before) = &mut self.before {
            before.clear();
        }
    }

    fn emit_changes(&self) {
        let Some(before) = &self.before else {
            return;
        };
        if before.is_empty() {
            return;
        }
        let mut senders = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        senders.retain(|sender| !sender.is_closed());
        if senders.is_empty() {
            return;
        }
        // Resolve only touched keys. A monotonic insertion ordinal preserves
        // source property order without scanning every retained row or history.
        let mut changed = {
            let revisions = self
                .revisions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            before
                .keys()
                .filter_map(|id| revisions.rows.get(id).map(|row| (row.ordinal, id)))
                .collect::<Vec<_>>()
        };
        changed.sort_unstable_by_key(|(ordinal, _)| *ordinal);
        for (_, id) in changed {
            let Some(state) = self.rows.get(id) else {
                continue;
            };
            let previous = before.get(id).and_then(Option::as_ref);
            let Some(after) = project(state, previous.is_none()) else {
                continue;
            };
            let event = match previous {
                None => after
                    .started
                    .expect("new lifecycle row includes its start event"),
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
            let old_status = previous.and_then(|row| {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::{LocalAgentTaskState, LocalBashTaskState, TaskStateBase, TaskStatus};

    fn shell(id: &str) -> TaskState {
        TaskState::LocalBash(LocalBashTaskState {
            base: TaskStateBase {
                id: id.into(),
                task_type: crate::id::TaskType::LocalBash,
                status: TaskStatus::Running,
                description: id.into(),
                tool_use_id: None,
                start_time: std::time::UNIX_EPOCH,
                end_time: None,
                total_paused_ms: 0,
                output_file: std::path::PathBuf::from(format!("/tmp/{id}.output")),
                evict_after: None,
                output_offset: 0,
                notified: false,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
            },
            is_adopted: false,
            caller: None,
            command: "sleep 1".into(),
            pid: None,
            exit_code: None,
            stop_cause: None,
            cwd: None,
            is_backgrounded: Some(true),
        })
    }

    fn agent(id: &str, agent_id: lingxi_core::types::AgentId) -> TaskState {
        let mut base = shell(id).base().clone();
        base.task_type = crate::id::TaskType::LocalAgent;
        TaskState::LocalAgent(LocalAgentTaskState {
            base,
            agent_id,
            agent_spawn_provenance: Default::default(),
            agent_list_lifecycle: Default::default(),
            spawned_description: None,
            handback: None,
            handback_history: vec![],
            is_parked: false,
            is_observer: false,
            observed_agent_id: None,
            subagent_type: "general-purpose".into(),
            prompt: "prompt".into(),
            error: None,
            messages: vec![],
            pending_messages: vec![],
            is_backgrounded: false,
            forked_skill_name: None,
            outcome: Default::default(),
        })
    }

    #[tokio::test]
    async fn keyed_writes_only_capture_touched_rows_and_reset_at_checkpoint() {
        let rows = TaskRows::default();
        for index in 0..128 {
            let id = format!("b{index}");
            rows.write().await.insert(id.clone(), shell(&id));
        }
        let mut events = rows.subscribe();
        {
            let mut write = rows.write().await;
            assert!(write.before.as_ref().unwrap().is_empty());
            assert!(write.get("b0").is_some());
            write.get_mut("b0").unwrap().base_mut().description = "updated".into();
            let captured = write.before.as_ref().unwrap();
            assert_eq!(captured.len(), 1);
            assert!(captured["b0"].as_ref().unwrap().started.is_none());
            write.checkpoint();
            assert!(write.before.as_ref().unwrap().is_empty());
            write.get_mut("b0").unwrap().base_mut().status = TaskStatus::Completed;
            assert_eq!(write.before.as_ref().unwrap().len(), 1);
        }
        let mut subtypes = vec![];
        while let Ok(event) = events.try_recv() {
            subtypes.push(event.string_at("/subtype").map(str::to_owned));
        }
        assert_eq!(
            subtypes,
            vec![
                Some("task_updated".into()),
                Some("task_updated".into()),
                Some("task_notification".into()),
                None,
            ]
        );
    }

    #[tokio::test]
    async fn non_lifecycle_agent_writes_skip_sdk_snapshots_and_keep_revisions() {
        use lingxi_core::host::task_registry::FieldPresence;
        use lingxi_core::types::{AgentId, ConversationMessage};

        let rows = TaskRows::default();
        let agent_id = AgentId::new();
        rows.write().await.insert("first", agent("first", agent_id));
        rows.write().await.insert("later", agent("later", agent_id));
        let mut events = rows.subscribe();
        let first = rows
            .ordered_find_map(|state, revision| Some((state.base().id.clone(), revision)))
            .await
            .unwrap();
        assert_eq!(first.0, "first", "selection keeps insertion order");
        {
            let mut write = rows.write().await;
            write.agent_local_facts_mut(agent_id).unwrap().is_idle = FieldPresence::Value(true);
            assert!(write.before.as_ref().unwrap().is_empty());
        }
        assert_eq!(
            rows.ordered_find_map(|_, revision| Some(revision)).await,
            Some(first.1),
            "a local-list fact does not invalidate transcript output"
        );
        {
            let mut write = rows.write().await;
            write
                .agent_transcript_mut("first")
                .unwrap()
                .push(ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "changed history".into(),
                ));
            assert!(write.before.as_ref().unwrap().is_empty());
        }
        let after = rows
            .ordered_find_map(|_, revision| Some(revision))
            .await
            .unwrap();
        assert_ne!(after, first.1);
        assert_eq!(
            rows.ordered_find_map(|_, revision| Some(revision)).await,
            Some(after),
            "authoritative reads reuse the unchanged transcript revision"
        );
        assert!(events.try_recv().is_err());
        rows.write()
            .await
            .get_mut("later")
            .unwrap()
            .base_mut()
            .status = TaskStatus::Failed;
        assert_eq!(
            rows.ordered_find_map(|_, revision| Some(revision)).await,
            Some(after),
            "unrelated row writes do not invalidate the selected transcript"
        );
        while let Ok(event) = events.try_recv() {
            assert_ne!(event.string_at("/task_id"), Some("first"));
        }
    }

    #[tokio::test]
    async fn mutable_iteration_and_removal_keep_events_and_order() {
        let rows = TaskRows::default();
        rows.write().await.insert("first", shell("first"));
        rows.write().await.insert("second", shell("second"));
        let mut events = rows.subscribe();
        {
            let mut write = rows.write().await;
            for state in write.values_mut() {
                state.base_mut().total_paused_ms = 7;
            }
            assert_eq!(write.before.as_ref().unwrap().len(), 2);
        }
        let mut updated = vec![];
        while let Ok(event) = events.try_recv() {
            assert_eq!(event.string_at("/subtype"), Some("task_updated"));
            updated.push(event.string_at("/task_id").unwrap().to_owned());
        }
        assert_eq!(updated, vec!["first", "second"]);
        {
            let mut write = rows.write().await;
            write.remove("first");
            write.insert("first", shell("first"));
        }
        assert_eq!(
            rows.ordered_find_map(|state, _| Some(state.base().id.clone()))
                .await,
            Some("second".into()),
            "a removed and reinserted key moves to the end"
        );
    }
}
