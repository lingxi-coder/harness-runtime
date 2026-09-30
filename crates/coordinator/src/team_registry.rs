//! Coordinator-side registry of teammate workers.
//!
//! Tracks each worker's metadata and status. The mailbox router (owned
//! by the registry) handles message delivery between coordinator and
//! teammates.

use crate::mailbox::{MailboxRouter, TeammateMailbox};
use lingxi_core::types::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;

/// One spawned worker tracked by the [`TeamRegistry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAgent {
    /// Awaiting a lead plan verdict, independent of the worker being idle.
    #[serde(default)]
    pub awaiting_plan_approval: bool,
    /// Worker agent ID.
    pub agent_id: AgentId,
    /// Agent-type string (e.g. "explorer", "writer").
    pub agent_type: String,
    /// Display name.
    pub name: String,
    /// ID of the spawning coordinator.
    pub parent_id: Option<AgentId>,
    /// Current status.
    pub status: WorkerStatus,
    /// Task ID associated with this worker.
    pub task_id: String,
    /// Spawn timestamp.
    pub spawned_at: SystemTime,
    /// Last-seen activity timestamp.
    pub last_active_at: SystemTime,
}

/// Lifecycle status of a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerStatus {
    /// Idle; no work in flight.
    Idle,
    /// Actively working — `activity` is a free-form description.
    Working {
        /// Human-readable activity.
        activity: String,
    },
    /// Blocked waiting for an inbound message.
    AwaitingMessage,
    /// Finished its task.
    Completed,
    /// Failed with `error`.
    Failed {
        /// Error message.
        error: String,
    },
    /// Killed by the coordinator or user.
    Killed,
}

/// Registry of teammate workers owned by a coordinator session.
pub struct TeamRegistry {
    workers: RwLock<HashMap<AgentId, WorkerAgent>>,
    permission_gate: RwLock<Option<Arc<dyn lingxi_core::host::PermissionGate>>>,
    message_forwarder:
        RwLock<Option<Arc<dyn lingxi_core::host::teammate_worker::PaneMessageForwarder>>>,
    /// Handler transitions that arrived after activation but before the teammate spawner
    /// linked the handler-generated task id to its worker. Access always follows
    /// the `workers` lock so linking and replay cannot miss each other.
    pending_handler_statuses: RwLock<HashMap<String, WorkerStatus>>,
    pending_plan_approvals: RwLock<HashMap<String, bool>>,
    /// Identities with an authoritative handler transition, including Idle.
    /// Accessed under the workers lock so startup cannot overwrite a new event.
    observed_handler_statuses: std::sync::Mutex<HashSet<AgentId>>,
    /// The coordinator agent's ID (parent of all workers in this registry).
    pub coordinator_id: AgentId,
    /// Shared mailbox router.
    pub mailbox_router: Arc<MailboxRouter>,
    /// Name of the single team this coordinator is running.
    ///
    /// Source of the reserved `ClientEvent::CoordinatorStatus { team }` DTO.
    /// We do not multiplex concurrent teams in this pass, so a single
    /// optional name suffices.
    team_name: RwLock<Option<String>>,
    /// Owning host's immutable storage root, independent of process env changes.
    config_home: Option<std::path::PathBuf>,
    approved_departures: RwLock<HashMap<String, Arc<PendingApprovedDeparture>>>,
}

/// A delivered shutdown approval whose departure waits for confirmed teardown.
pub(crate) struct PendingApprovedDeparture {
    pub home: Option<std::path::PathBuf>,
    pub team_name: String,
    pub list_id: String,
    pub agent_id: AgentId,
    pub worker_name: String,
    pub leader: AgentId,
    pub from: crate::mailbox::MessageSender,
    pub from_name: String,
    pub request_id: String,
    pub progress: tokio::sync::Mutex<ApprovedDepartureProgress>,
}

#[derive(Default)]
pub(crate) struct ApprovedDepartureProgress {
    member_removed: bool,
    unassigned: Vec<task_store::UnassignedTask>,
    tasks_done: bool,
    notified: bool,
}

impl PendingApprovedDeparture {
    async fn complete(&self, router: &MailboxRouter) -> Result<(), String> {
        let mut progress = self.progress.lock().await;
        if progress.notified {
            return Ok(());
        }
        let agent_id = self.agent_id.to_string();
        if !progress.member_removed {
            if let Some(home) = &self.home {
                crate::team_file::remove_team_member(
                    home,
                    &self.team_name,
                    &agent_id,
                    &self.worker_name,
                )
                .await
                .map_err(|error| format!("Remove teammate membership: {error}"))?;
            }
            progress.member_removed = true;
        }
        if !progress.tasks_done {
            if let Some(home) = &self.home {
                let result = task_store::TodoStore::for_list_at(home, &self.list_id)
                    .try_unassign_tasks_for_teammate(
                        &agent_id,
                        &self.worker_name,
                        task_store::TeammateEndReason::Shutdown,
                    )
                    .await;
                let (unassigned, error) = match result {
                    Ok(outcome) => (outcome.unassigned_tasks, None),
                    Err(error) => (error.unassigned_tasks, Some(error.source.to_string())),
                };
                for task in unassigned {
                    if !progress.unassigned.iter().any(|saved| saved.id == task.id) {
                        progress.unassigned.push(task);
                    }
                }
                progress.unassigned.sort_by(|left, right| {
                    left.id
                        .len()
                        .cmp(&right.id.len())
                        .then_with(|| left.id.cmp(&right.id))
                });
                if let Some(error) = error {
                    return Err(format!("Unassign teammate tasks: {error}"));
                }
            }
            progress.tasks_done = true;
        }
        let notification_message = task_store::format_unassignment_notification(
            &self.worker_name,
            task_store::TeammateEndReason::Shutdown,
            &progress.unassigned,
        );
        router.route(&self.leader, crate::mailbox::TeammateMessage {
            from: self.from.clone(), from_name: self.from_name.clone(),
            content: serde_json::json!({"type":"teammate_terminated", "message":notification_message}).to_string(),
            summary: None, message_id: tool_api::util::ids::ulid_or_uuid(), timestamp: SystemTime::now(),
            request_id: Some(self.request_id.clone()),
        }).await.map_err(|error| format!("Notify teammate departure: {error}"))?;
        progress.notified = true;
        Ok(())
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::team_spawn::TeammateDepartureCleanup for TeamRegistry {
    async fn has_pending_departure(&self, task_id: &str) -> bool {
        let entry = self.approved_departures.read().await.get(task_id).cloned();
        match entry {
            Some(entry) => !entry.progress.lock().await.notified,
            None => false,
        }
    }

    async fn complete_departure(&self, task_id: &str) -> Result<(), String> {
        self.complete_approved_departure(task_id).await
    }
}

impl TeamRegistry {
    /// Construct an empty registry owned by `coordinator_id`.
    #[must_use]
    pub fn new(coordinator_id: AgentId) -> Self {
        Self {
            workers: RwLock::new(HashMap::new()),
            permission_gate: RwLock::new(None),
            message_forwarder: RwLock::new(None),
            pending_handler_statuses: RwLock::new(HashMap::new()),
            pending_plan_approvals: RwLock::new(HashMap::new()),
            observed_handler_statuses: std::sync::Mutex::new(HashSet::new()),
            coordinator_id,
            mailbox_router: Arc::new(MailboxRouter::new()),
            team_name: RwLock::new(None),
            config_home: crate::team_file::lingxi_home(),
            approved_departures: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) async fn register_approved_departure(
        &self,
        task_id: &str,
        departure: PendingApprovedDeparture,
    ) {
        self.approved_departures
            .write()
            .await
            .entry(task_id.to_owned())
            .or_insert_with(|| Arc::new(departure));
    }

    /// Finish confirmed departure, retaining successful steps across I/O retries.
    /// An owned operation survives caller cancellation; its progress lock also
    /// serializes competing reader, Stop, and SendMessage completion paths.
    pub async fn complete_approved_departure(&self, task_id: &str) -> Result<(), String> {
        let Some(departure) = self.approved_departures.read().await.get(task_id).cloned() else {
            return Ok(());
        };
        let router = self.mailbox_router.clone();
        tokio::spawn(async move { departure.complete(&router).await })
            .await
            .map_err(|error| format!("Departure cleanup task failed: {error}"))?
    }

    /// Bind this registry to the owning host's storage root before sharing it.
    #[must_use]
    pub fn with_config_home(mut self, home: std::path::PathBuf) -> Self {
        self.config_home = Some(home);
        self
    }

    #[must_use]
    pub fn config_home(&self) -> Option<&std::path::Path> {
        self.config_home.as_deref()
    }

    /// Live leader gate used to derive a teammate's approved permission mode.
    pub async fn set_permission_gate(&self, gate: Arc<dyn lingxi_core::host::PermissionGate>) {
        *self.permission_gate.write().await = Some(gate);
    }
    pub async fn permission_gate(&self) -> Option<Arc<dyn lingxi_core::host::PermissionGate>> {
        self.permission_gate.read().await.clone()
    }

    /// Spawn (i.e. register) a new worker.
    pub async fn spawn_worker(
        &self,
        agent_type: String,
        name: String,
        task_id: String,
    ) -> Result<AgentId, crate::mailbox::MailboxError> {
        let agent_id = AgentId::new();
        self.register_worker(agent_id, agent_type, name, task_id)
            .await?;
        Ok(agent_id)
    }

    /// Register a worker whose identity is assigned by a parent process.
    pub async fn register_worker(
        &self,
        agent_id: AgentId,
        agent_type: String,
        name: String,
        task_id: String,
    ) -> Result<(), crate::mailbox::MailboxError> {
        let mailbox = Arc::new(TeammateMailbox::new(agent_id));
        self.mailbox_router.register(agent_id, mailbox).await;
        // Index the worker's display name so a teammate can be addressed by
        // NAME (claude-code's mailbox is name-keyed; `TaskUpdate` owner-change
        // notifications and `getAgentStatuses` address recipients by name).
        self.mailbox_router.register_name(&name, agent_id).await;
        self.workers.write().await.insert(
            agent_id,
            WorkerAgent {
                awaiting_plan_approval: false,
                agent_id,
                agent_type,
                name,
                parent_id: Some(self.coordinator_id),
                status: WorkerStatus::Idle,
                task_id,
                spawned_at: SystemTime::now(),
                last_active_at: SystemTime::now(),
            },
        );
        Ok(())
    }

    /// Install the parent-process message bridge for a pane worker.
    pub async fn set_message_forwarder(
        &self,
        forwarder: Arc<dyn lingxi_core::host::teammate_worker::PaneMessageForwarder>,
    ) {
        *self.message_forwarder.write().await = Some(forwarder);
    }

    pub async fn message_forwarder(
        &self,
    ) -> Option<Arc<dyn lingxi_core::host::teammate_worker::PaneMessageForwarder>> {
        self.message_forwarder.read().await.clone()
    }

    /// Remove a worker and unregister its mailbox.
    pub async fn delete_worker(&self, agent_id: &AgentId) {
        {
            let mut workers = self.workers.write().await;
            workers.remove(agent_id);
            self.observed_handler_statuses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(agent_id);
            let mut pending = self.pending_handler_statuses.write().await;
            if workers.values().all(|worker| !worker.task_id.is_empty()) {
                pending.clear();
                self.pending_plan_approvals.write().await.clear();
            }
        }
        self.mailbox_router.unregister(agent_id).await;
    }

    /// List all currently registered workers.
    pub async fn list(&self) -> Vec<WorkerAgent> {
        self.workers.read().await.values().cloned().collect()
    }

    /// Update a worker's status and touch its `last_active_at` timestamp.
    ///
    /// No-op (no panic) if no worker with `agent_id` is registered.
    pub async fn update_status(&self, agent_id: &AgentId, status: WorkerStatus) {
        if let Some(worker) = self.workers.write().await.get_mut(agent_id) {
            worker.status = status;
            if worker_terminal(&worker.status) {
                worker.awaiting_plan_approval = false;
            }
            worker.last_active_at = SystemTime::now();
        }
    }

    /// Initialize a worker only before any authoritative handler transition.
    ///
    /// Team startup uses this to publish its initial derived status without
    /// racing a concurrent handler failure and resurrecting that worker as
    /// active, or overwriting an already-published Idle. The check and write
    /// intentionally share one registry lock.
    pub async fn update_status_if_nonterminal(&self, agent_id: &AgentId, status: WorkerStatus) {
        if let Some(worker) = self.workers.write().await.get_mut(agent_id) {
            if self
                .observed_handler_statuses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(agent_id)
            {
                return;
            }
            if matches!(
                &worker.status,
                WorkerStatus::Completed | WorkerStatus::Failed { .. } | WorkerStatus::Killed
            ) {
                return;
            }
            worker.status = status;
            if worker_terminal(&worker.status) {
                worker.awaiting_plan_approval = false;
            }
            worker.last_active_at = SystemTime::now();
        }
    }

    /// Apply a lifecycle status emitted by a teammate handler.
    ///
    /// The first terminal state wins, except that a later `Failed` payload may
    /// replace an earlier generic `Failed` sentinel with the real error reason.
    /// The check and write share the registry lock so a concurrent kill cannot
    /// be resurrected as `Failed`, `Completed`, or `Working`.
    pub async fn update_status_from_handler(&self, agent_id: &AgentId, status: WorkerStatus) {
        if let Some(worker) = self.workers.write().await.get_mut(agent_id) {
            self.observed_handler_statuses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(*agent_id);
            if apply_handler_transition(&mut worker.status, status) {
                if worker_terminal(&worker.status) {
                    worker.awaiting_plan_approval = false;
                }
                worker.last_active_at = SystemTime::now();
            }
        }
    }

    /// Apply a handler transition by task id, or retain it until the teammate spawner
    /// publishes the worker↔task link.
    ///
    /// Returns `true` when the task id already resolved to a worker. A transition
    /// is buffered only while at least one unlinked worker exists; genuinely
    /// unknown ids remain no-ops instead of growing an unbounded pending map.
    pub async fn update_status_from_handler_by_task_id(
        &self,
        task_id: &str,
        status: WorkerStatus,
    ) -> bool {
        if task_id.is_empty() {
            return false;
        }

        let mut workers = self.workers.write().await;
        if let Some(worker) = workers
            .values_mut()
            .find(|worker| worker.task_id == task_id)
        {
            self.observed_handler_statuses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(worker.agent_id);
            if apply_handler_transition(&mut worker.status, status) {
                if worker_terminal(&worker.status) {
                    worker.awaiting_plan_approval = false;
                }
                worker.last_active_at = SystemTime::now();
            }
            return true;
        }
        if !workers.values().any(|worker| worker.task_id.is_empty()) {
            return false;
        }

        let mut pending = self.pending_handler_statuses.write().await;
        if let Some(current) = pending.get_mut(task_id) {
            apply_handler_transition(current, status);
        } else {
            pending.insert(task_id.to_string(), status);
        }
        false
    }

    pub async fn set_awaiting_plan_approval(&self, task_id: &str, awaiting: bool) {
        if task_id.is_empty() {
            return;
        }
        let mut workers = self.workers.write().await;
        if let Some(worker) = workers
            .values_mut()
            .find(|worker| worker.task_id == task_id)
        {
            worker.awaiting_plan_approval = awaiting && !worker_terminal(&worker.status);
        } else if workers.values().any(|worker| worker.task_id.is_empty()) {
            self.pending_plan_approvals
                .write()
                .await
                .insert(task_id.into(), awaiting);
        }
    }

    /// Write back the handler-generated task id onto a worker.
    ///
    /// No-op (no panic) if no worker with `agent_id` is registered.
    pub async fn set_task_id(&self, agent_id: &AgentId, task_id: String) {
        let mut workers = self.workers.write().await;
        if let Some(worker) = workers.get_mut(agent_id) {
            let mut pending = self.pending_handler_statuses.write().await;
            if let Some(status) = pending.remove(&task_id) {
                self.observed_handler_statuses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(*agent_id);
                apply_handler_transition(&mut worker.status, status);
            }
            if let Some(awaiting) = self.pending_plan_approvals.write().await.remove(&task_id) {
                worker.awaiting_plan_approval = awaiting && !worker_terminal(&worker.status);
            }
            if worker_terminal(&worker.status) {
                worker.awaiting_plan_approval = false;
            }
            worker.task_id = task_id;
            worker.last_active_at = SystemTime::now();
            if workers.values().all(|worker| !worker.task_id.is_empty()) {
                pending.clear();
                self.pending_plan_approvals.write().await.clear();
            }
        }
    }

    /// Find a worker by its (handler-generated) task id.
    ///
    /// Linear scan over the workers map — acceptable at the per-session team
    /// scale (a handful of workers); avoids a second index to keep in sync
    /// with `set_task_id` / `delete_worker`. An empty `task_id` never matches
    /// a freshly-spawned, not-yet-linked worker (their `task_id` is empty too,
    /// so callers must not look up the empty string).
    pub async fn find_by_task_id(&self, task_id: &str) -> Option<WorkerAgent> {
        if task_id.is_empty() {
            return None;
        }
        self.workers
            .read()
            .await
            .values()
            .find(|w| w.task_id == task_id)
            .cloned()
    }

    /// Resolve a worker by its display `name` (case-insensitive).
    ///
    /// Coordinator-side `SendMessage` accepts a bare teammate name as the
    /// recipient (TS `to`), which it resolves to the worker's [`AgentId`] before
    /// routing. Linear scan over the workers map — acceptable at per-session team
    /// scale (a handful of workers), mirroring [`Self::find_by_task_id`]. An
    /// empty `name` never matches.
    pub async fn find_by_name(&self, name: &str) -> Option<WorkerAgent> {
        if name.is_empty() {
            return None;
        }
        self.workers
            .read()
            .await
            .values()
            .find(|w| w.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// Resolve a worker by its [`AgentId`] — an O(1) map lookup.
    ///
    /// The sender-name / agent-type resolutions on the `SendMessage`,
    /// `SyntheticOutput` and teammate-definition paths all key on exactly the
    /// `AgentId` this map is indexed by. Going through [`Self::list`] there
    /// deep-clones EVERY worker record (four owned `String`s apiece) just to
    /// read one field off one of them, on every message.
    pub async fn find_by_agent_id(&self, agent_id: &AgentId) -> Option<WorkerAgent> {
        self.workers.read().await.get(agent_id).cloned()
    }

    /// Set (or clear) the team name for this coordinator session.
    pub async fn set_team_name(&self, name: Option<String>) {
        *self.team_name.write().await = name;
    }

    /// Read the current team name, if any.
    pub async fn team_name(&self) -> Option<String> {
        self.team_name.read().await.clone()
    }

    /// Count of workers whose status is non-terminal.
    ///
    /// Non-terminal = `Idle` | `Working { .. }` | `AwaitingMessage`.
    /// Terminal (excluded) = `Completed` | `Failed { .. }` | `Killed`.
    pub async fn active_worker_count(&self) -> u32 {
        let count = self
            .workers
            .read()
            .await
            .values()
            .filter(|w| {
                matches!(
                    w.status,
                    WorkerStatus::Idle
                        | WorkerStatus::Working { .. }
                        | WorkerStatus::AwaitingMessage
                )
            })
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

/// Apply the handler lifecycle's first-terminal-wins rule to one status value.
/// A `Failed`→`Failed` transition is the intentional sentinel→real-reason
/// upgrade used by [`TeamRegistry::update_status_from_handler`].
fn worker_terminal(status: &WorkerStatus) -> bool {
    matches!(
        status,
        WorkerStatus::Completed | WorkerStatus::Failed { .. } | WorkerStatus::Killed
    )
}

fn apply_handler_transition(current: &mut WorkerStatus, status: WorkerStatus) -> bool {
    let current_is_terminal = matches!(
        current,
        WorkerStatus::Completed | WorkerStatus::Failed { .. } | WorkerStatus::Killed
    );
    let failed_reason_upgrade = matches!(
        (&*current, &status),
        (WorkerStatus::Failed { .. }, WorkerStatus::Failed { .. })
    );
    if current_is_terminal && !failed_reason_upgrade {
        return false;
    }
    *current = status;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approved_departure_accumulates_committed_tasks_across_retries() {
        let home = tempfile::tempdir().unwrap();
        let leader = AgentId::new();
        let registry = TeamRegistry::new(leader);
        let mailbox = Arc::new(TeammateMailbox::new(leader));
        registry
            .mailbox_router
            .register(leader, mailbox.clone())
            .await;
        let store = task_store::TodoStore::for_list_at(home.path(), "session");
        let mut ids = Vec::new();
        for subject in ["first", "second"] {
            let mut task = task_store::TodoTask::new(
                subject.into(),
                String::new(),
                None,
                serde_json::Map::new(),
            );
            task.owner = Some("nova".into());
            ids.push(store.create(task).await.unwrap());
        }
        registry
            .register_approved_departure(
                "pane-task",
                PendingApprovedDeparture {
                    home: Some(home.path().to_owned()),
                    team_name: "session".into(),
                    list_id: "session".into(),
                    agent_id: AgentId::new(),
                    worker_name: "nova".into(),
                    leader,
                    from: crate::mailbox::MessageSender::Teammate(AgentId::new()),
                    from_name: "nova".into(),
                    request_id: "approval-1".into(),
                    progress: tokio::sync::Mutex::new(ApprovedDepartureProgress::default()),
                },
            )
            .await;
        let held = task_store::proper_lockfile::lock(
            &home
                .path()
                .join("tasks/session")
                .join(format!("{}.json", ids[1])),
        )
        .await
        .unwrap();
        assert!(registry
            .complete_approved_departure("pane-task")
            .await
            .is_err());
        assert!(mailbox.drain().is_empty());
        assert_eq!(store.get(&ids[0]).await.unwrap().owner, None);
        assert_eq!(
            store.get(&ids[1]).await.unwrap().owner.as_deref(),
            Some("nova")
        );
        drop(held);
        registry
            .complete_approved_departure("pane-task")
            .await
            .unwrap();
        registry
            .complete_approved_departure("pane-task")
            .await
            .unwrap();
        let messages = mailbox.drain();
        assert_eq!(messages.len(), 1);
        let expected = task_store::format_unassignment_notification(
            "nova",
            task_store::TeammateEndReason::Shutdown,
            &[
                task_store::UnassignedTask {
                    id: ids[0].clone(),
                    subject: "first".into(),
                },
                task_store::UnassignedTask {
                    id: ids[1].clone(),
                    subject: "second".into(),
                },
            ],
        );
        let value: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert_eq!(value["message"], expected);
    }

    #[tokio::test]
    async fn approved_departure_retries_after_team_file_lock_recovers() {
        let home = tempfile::tempdir().unwrap();
        let leader = AgentId::new();
        let registry = TeamRegistry::new(leader);
        let mailbox = Arc::new(TeammateMailbox::new(leader));
        registry
            .mailbox_router
            .register(leader, mailbox.clone())
            .await;
        let path = crate::team_file::team_file_path(home.path(), "session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"members":[{"name":"nova"}]}"#).unwrap();
        registry
            .register_approved_departure(
                "pane-task",
                PendingApprovedDeparture {
                    home: Some(home.path().to_owned()),
                    team_name: "session".into(),
                    list_id: "session".into(),
                    agent_id: AgentId::new(),
                    worker_name: "nova".into(),
                    leader,
                    from: crate::mailbox::MessageSender::Teammate(AgentId::new()),
                    from_name: "nova".into(),
                    request_id: "approval-1".into(),
                    progress: tokio::sync::Mutex::new(ApprovedDepartureProgress::default()),
                },
            )
            .await;
        let held = task_store::proper_lockfile::lock(&path).await.unwrap();
        assert!(registry
            .complete_approved_departure("pane-task")
            .await
            .is_err());
        assert!(mailbox.drain().is_empty());
        assert!(std::fs::read_to_string(&path).unwrap().contains("nova"));
        drop(held);
        registry
            .complete_approved_departure("pane-task")
            .await
            .unwrap();
        registry
            .complete_approved_departure("pane-task")
            .await
            .unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(document["members"], serde_json::json!([]));
        assert_eq!(mailbox.drain().len(), 1);
    }

    #[tokio::test]
    async fn approved_departure_completion_is_shared_and_exactly_once() {
        let leader = AgentId::new();
        let registry = TeamRegistry::new(leader);
        let mailbox = Arc::new(TeammateMailbox::new(leader));
        registry
            .mailbox_router
            .register(leader, mailbox.clone())
            .await;
        let agent_id = AgentId::new();
        registry
            .register_approved_departure(
                "pane-task",
                PendingApprovedDeparture {
                    home: None,
                    team_name: "session".into(),
                    list_id: "session".into(),
                    agent_id,
                    worker_name: "nova".into(),
                    leader,
                    from: crate::mailbox::MessageSender::Teammate(agent_id),
                    from_name: "nova".into(),
                    request_id: "approval-1".into(),
                    progress: tokio::sync::Mutex::new(ApprovedDepartureProgress::default()),
                },
            )
            .await;
        assert!(
            mailbox.drain().is_empty(),
            "registration must not announce departure before teardown"
        );
        let (first, second) = tokio::join!(
            registry.complete_approved_departure("pane-task"),
            registry.complete_approved_departure("pane-task")
        );
        first.unwrap();
        second.unwrap();
        registry
            .complete_approved_departure("pane-task")
            .await
            .unwrap();
        let messages = mailbox.drain();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].request_id.as_deref(), Some("approval-1"));
        assert_eq!(messages[0].from_name, "nova");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&messages[0].content).unwrap(),
            serde_json::json!({"type":"teammate_terminated", "message":"nova has shut down."})
        );
    }

    #[tokio::test]
    async fn spawn_then_update_status_transitions() {
        let reg = TeamRegistry::new(AgentId::new());
        let id = reg
            .spawn_worker("explorer".into(), "alpha".into(), String::new())
            .await
            .unwrap();

        // Freshly spawned worker is Idle.
        let before = reg.list().await;
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].status, WorkerStatus::Idle);
        let last_active_before = before[0].last_active_at;

        // Sleep a hair so the monotonic clock can advance.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        reg.update_status(
            &id,
            WorkerStatus::Working {
                activity: "running".into(),
            },
        )
        .await;

        let after = reg.list().await;
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].status,
            WorkerStatus::Working {
                activity: "running".into()
            }
        );
        assert!(
            after[0].last_active_at >= last_active_before,
            "last_active_at must advance (or stay equal) on update_status"
        );
    }

    #[tokio::test]
    async fn update_status_failed_and_killed() {
        let reg = TeamRegistry::new(AgentId::new());
        let id = reg
            .spawn_worker("writer".into(), "beta".into(), String::new())
            .await
            .unwrap();

        reg.update_status(
            &id,
            WorkerStatus::Failed {
                error: "boom".into(),
            },
        )
        .await;
        assert_eq!(
            reg.list().await[0].status,
            WorkerStatus::Failed {
                error: "boom".into()
            }
        );

        reg.update_status(&id, WorkerStatus::Killed).await;
        assert_eq!(reg.list().await[0].status, WorkerStatus::Killed);
    }

    #[tokio::test]
    async fn startup_does_not_overwrite_linked_handler_idle() {
        let registry = TeamRegistry::new(AgentId::new());
        let id = registry
            .spawn_worker(
                "general-purpose".into(),
                "quick".into(),
                "task-quick".into(),
            )
            .await
            .unwrap();
        registry
            .update_status_from_handler_by_task_id("task-quick", WorkerStatus::Idle)
            .await;
        registry
            .update_status_if_nonterminal(
                &id,
                WorkerStatus::Working {
                    activity: "running".into(),
                },
            )
            .await;
        assert_eq!(
            registry.find_by_agent_id(&id).await.unwrap().status,
            WorkerStatus::Idle
        );
        registry.delete_worker(&id).await;
        registry
            .register_worker(
                id,
                "general-purpose".into(),
                "quick".into(),
                "new-task".into(),
            )
            .await
            .unwrap();
        registry
            .update_status_if_nonterminal(
                &id,
                WorkerStatus::Working {
                    activity: "running".into(),
                },
            )
            .await;
        assert!(matches!(
            registry.find_by_agent_id(&id).await.unwrap().status,
            WorkerStatus::Working { .. }
        ));
    }

    #[tokio::test]
    async fn startup_status_does_not_overwrite_a_concurrent_terminal_status() {
        let reg = TeamRegistry::new(AgentId::new());
        let id = reg
            .spawn_worker("writer".into(), "beta".into(), String::new())
            .await
            .unwrap();
        reg.update_status(
            &id,
            WorkerStatus::Failed {
                error: "provider failed".into(),
            },
        )
        .await;

        reg.update_status_if_nonterminal(
            &id,
            WorkerStatus::Working {
                activity: "running".into(),
            },
        )
        .await;

        assert_eq!(
            reg.list().await[0].status,
            WorkerStatus::Failed {
                error: "provider failed".into()
            }
        );
    }

    #[tokio::test]
    async fn handler_status_preserves_killed_but_can_upgrade_a_failed_reason() {
        let reg = TeamRegistry::new(AgentId::new());
        let id = reg
            .spawn_worker("writer".into(), "beta".into(), String::new())
            .await
            .unwrap();

        reg.update_status(&id, WorkerStatus::Killed).await;
        reg.update_status_from_handler(
            &id,
            WorkerStatus::Failed {
                error: "late provider failure".into(),
            },
        )
        .await;
        assert_eq!(reg.list().await[0].status, WorkerStatus::Killed);

        reg.update_status(
            &id,
            WorkerStatus::Failed {
                error: "teammate task failed".into(),
            },
        )
        .await;
        reg.update_status_from_handler(
            &id,
            WorkerStatus::Failed {
                error: "real provider reason".into(),
            },
        )
        .await;
        assert_eq!(
            reg.list().await[0].status,
            WorkerStatus::Failed {
                error: "real provider reason".into()
            }
        );
    }

    #[tokio::test]
    async fn find_by_task_id_roundtrip() {
        let reg = TeamRegistry::new(AgentId::new());
        let id = reg
            .spawn_worker("explorer".into(), "gamma".into(), String::new())
            .await
            .unwrap();

        // Not yet linked: empty task_id should not resolve to this worker.
        assert!(reg.find_by_task_id("task-xyz").await.is_none());

        reg.set_task_id(&id, "task-xyz".into()).await;
        let found = reg.find_by_task_id("task-xyz").await;
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.agent_id, id);
        assert_eq!(found.task_id, "task-xyz");

        // Unknown id resolves to nothing.
        assert!(reg.find_by_task_id("nope").await.is_none());
    }

    #[tokio::test]
    async fn linking_last_unlinked_worker_clears_unmatched_pending_statuses() {
        let reg = TeamRegistry::new(AgentId::new());
        let id = reg
            .spawn_worker("explorer".into(), "gamma".into(), String::new())
            .await
            .unwrap();

        assert!(
            !reg.update_status_from_handler_by_task_id(
                "late-deleted-task",
                WorkerStatus::Failed {
                    error: "late failure".into(),
                },
            )
            .await
        );
        assert_eq!(reg.pending_handler_statuses.read().await.len(), 1);

        reg.set_task_id(&id, "actual-task".into()).await;

        assert!(reg.pending_handler_statuses.read().await.is_empty());
        assert_eq!(reg.list().await[0].status, WorkerStatus::Idle);
    }

    #[tokio::test]
    async fn deleting_last_unlinked_worker_clears_unmatched_pending_statuses() {
        let reg = TeamRegistry::new(AgentId::new());
        let first = reg
            .spawn_worker("explorer".into(), "alpha".into(), String::new())
            .await
            .unwrap();
        let second = reg
            .spawn_worker("writer".into(), "beta".into(), String::new())
            .await
            .unwrap();
        reg.update_status_from_handler_by_task_id("late-deleted-task", WorkerStatus::Killed)
            .await;

        reg.delete_worker(&first).await;
        assert_eq!(
            reg.pending_handler_statuses.read().await.len(),
            1,
            "pending statuses remain while another worker can still link"
        );

        reg.delete_worker(&second).await;
        assert!(reg.pending_handler_statuses.read().await.is_empty());
    }

    #[tokio::test]
    async fn active_worker_count_excludes_terminal() {
        let reg = TeamRegistry::new(AgentId::new());
        let working = reg
            .spawn_worker("a".into(), "w-working".into(), String::new())
            .await
            .unwrap();
        let idle = reg
            .spawn_worker("a".into(), "w-idle".into(), String::new())
            .await
            .unwrap();
        let awaiting = reg
            .spawn_worker("a".into(), "w-awaiting".into(), String::new())
            .await
            .unwrap();
        let failed = reg
            .spawn_worker("a".into(), "w-failed".into(), String::new())
            .await
            .unwrap();
        let killed = reg
            .spawn_worker("a".into(), "w-killed".into(), String::new())
            .await
            .unwrap();
        let completed = reg
            .spawn_worker("a".into(), "w-completed".into(), String::new())
            .await
            .unwrap();

        reg.update_status(
            &working,
            WorkerStatus::Working {
                activity: "running".into(),
            },
        )
        .await;
        // `idle` stays Idle (spawned default).
        let _ = idle;
        reg.update_status(&awaiting, WorkerStatus::AwaitingMessage)
            .await;
        reg.update_status(&failed, WorkerStatus::Failed { error: "x".into() })
            .await;
        reg.update_status(&killed, WorkerStatus::Killed).await;
        reg.update_status(&completed, WorkerStatus::Completed).await;

        // Non-terminal = Idle | Working | AwaitingMessage => 3.
        // Terminal (excluded) = Completed | Failed | Killed.
        assert_eq!(reg.active_worker_count().await, 3);
    }

    #[tokio::test]
    async fn team_name_set_get() {
        let reg = TeamRegistry::new(AgentId::new());
        assert_eq!(reg.team_name().await, None);

        reg.set_team_name(Some("alpha".into())).await;
        assert_eq!(reg.team_name().await, Some("alpha".to_string()));

        reg.set_team_name(None).await;
        assert_eq!(reg.team_name().await, None);
    }

    #[tokio::test]
    async fn update_status_unknown_agent_is_noop() {
        let reg = TeamRegistry::new(AgentId::new());
        // No worker registered: must not panic.
        reg.update_status(&AgentId::new(), WorkerStatus::Killed)
            .await;
        assert!(reg.list().await.is_empty());
    }
    #[tokio::test]
    async fn plan_review_flag_replays_before_link_and_keeps_idle_separate() {
        let registry = TeamRegistry::new(AgentId::new());
        let worker = registry
            .spawn_worker("planner".into(), "planner".into(), String::new())
            .await
            .unwrap();
        registry.set_awaiting_plan_approval("task-plan", true).await;
        registry.set_task_id(&worker, "task-plan".into()).await;
        let row = registry.find_by_agent_id(&worker).await.unwrap();
        assert!(row.awaiting_plan_approval);
        assert_eq!(row.status, WorkerStatus::Idle);
        registry
            .set_awaiting_plan_approval("task-plan", false)
            .await;
        let row = registry.find_by_agent_id(&worker).await.unwrap();
        assert!(!row.awaiting_plan_approval);
        assert_eq!(row.status, WorkerStatus::Idle);
        registry.set_awaiting_plan_approval("task-plan", true).await;
        registry
            .update_status_from_handler(&worker, WorkerStatus::Killed)
            .await;
        registry.set_awaiting_plan_approval("task-plan", true).await;
        assert!(
            !registry
                .find_by_agent_id(&worker)
                .await
                .unwrap()
                .awaiting_plan_approval
        );
    }
}
