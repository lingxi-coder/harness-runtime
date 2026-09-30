//! Trusted human inboxes. Model tools never acquire this origin or stop epoch.
use super::*;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct HumanInbox {
    epoch: u64,
    messages: VecDeque<(u64, u64, String)>,
    next_message: u64,
    resume: Arc<tokio::sync::Mutex<()>>,
    cancel: tokio_util::sync::CancellationToken,
}
impl TaskRegistry {
    pub(crate) fn invalidate_human_messages(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut inboxes = self.human_messages.lock().unwrap();
        let inbox = inboxes.entry(id.to_owned()).or_default();
        inbox.epoch = inbox.epoch.wrapping_add(1);
        inbox.messages.clear();
        inbox.cancel.cancel();
        inbox.cancel = tokio_util::sync::CancellationToken::new();
        inbox.resume.clone()
    }

    pub(crate) fn has_human_messages(&self, id: &str) -> bool {
        self.human_messages
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|inbox| {
                inbox
                    .messages
                    .iter()
                    .any(|(epoch, _, _)| *epoch == inbox.epoch)
            })
    }

    pub async fn send_human_task_message(&self, id: &str, message: &str) -> Result<(), TaskError> {
        // A host can cancel the request future when its turn is replaced. The
        // accepted human resume must nevertheless finish its startup or its
        // rollback, otherwise it can leave a live worker behind a released
        // resume gate. Run the transaction in an owned task when the registry
        // was composed through an `Arc`.
        if let Some(registry) = self.owned_self() {
            let id = id.to_owned();
            let message = message.to_owned();
            return tokio::spawn(async move {
                registry.send_human_task_message_inner(&id, &message).await
            })
            .await
            .map_err(|error| {
                TaskError::Internal(format!("human resume transaction joined: {error}"))
            })?;
        }
        self.send_human_task_message_inner(id, message).await
    }

    async fn send_human_task_message_inner(
        &self,
        id: &str,
        message: &str,
    ) -> Result<(), TaskError> {
        if message.trim().is_empty() {
            return Err(TaskError::Internal("message must not be empty".into()));
        }
        let id = self.canonical_or_raw(id).await;
        let agent_id = match self.get(&id).await {
            Some(TaskState::LocalAgent(agent)) if !agent.is_observer => agent.agent_id,
            Some(_) => return Err(TaskError::Unsupported),
            None => return Err(TaskError::NotFound(id)),
        };
        let (epoch, message_id, resume, cancel) = {
            let mut inboxes = self.human_messages.lock().unwrap();
            let inbox = inboxes.entry(id.clone()).or_default();
            let message_id = inbox.next_message;
            inbox.next_message = inbox.next_message.wrapping_add(1);
            inbox
                .messages
                .push_back((inbox.epoch, message_id, message.to_owned()));
            (
                inbox.epoch,
                message_id,
                inbox.resume.clone(),
                inbox.cancel.clone(),
            )
        };
        self.bump_notification_revision();
        let _resume = tokio::select! {
            _ = cancel.cancelled() => return Err(TaskError::TerminatedTask),
            guard = resume.lock() => guard,
        };
        let live = self.get(&id).await.is_some_and(|state| matches!(&state,
            TaskState::LocalAgent(agent) if !state.is_terminated() && agent.outcome.killed_by.as_deref() != Some("user")));
        if live && self.agent_message_receivers.lock().await.contains_key(&id) {
            return Ok(());
        }
        let handler = match self.handler_for(TaskType::LocalAgent).await {
            Some(handler) => handler,
            None => {
                if let Some(inbox) = self.human_messages.lock().unwrap().get_mut(&id) {
                    inbox
                        .messages
                        .retain(|(_, queued_id, _)| *queued_id != message_id);
                }
                return Err(TaskError::Unsupported);
            }
        };
        if live && handler.has_live_worker(&id).await {
            return Ok(());
        }
        let previous = self.get(&id).await;
        let result = async {
            let mut prepared = tokio::select! {
                _ = cancel.cancelled() => return Err(TaskError::TerminatedTask),
                prepared = handler.prepare_human_resume(&id, agent_id, epoch, TaskContext {
                    fs: self.fs.clone(), runtime: self.runtime.clone(),
                }) => prepared?,
            };
            {
                let mut rows = self.tasks.write().await;
                let mut routes = self.spawned.write().await;
                let mut cleanups = self.cleanups.lock().await;
                let mut receivers = self.agent_message_receivers.lock().await;
                if cancel.is_cancelled() || !rows.contains_key(&id) { return Err(TaskError::TerminatedTask); }
                if let Some(cleanup) = prepared.handle.cleanup.clone() { cleanups.insert(id.clone(), cleanup); }
                routes.insert(id.clone(), TaskType::LocalAgent);
                receivers.remove(&id);
                // Startup observer performs the epoch-checked status transition.
                let _ = &mut rows;
            }
            prepared.handle.activate();
            tokio::select! {
                _ = cancel.cancelled() => Err(TaskError::TerminatedTask),
                ready = prepared.ready => ready.map_err(|_| TaskError::Internal("human resume startup was cancelled".into()))?,
            }
        }.await;
        if result.is_err() {
            let mut rows = self.tasks.write().await;
            let mut inboxes = self.human_messages.lock().unwrap();
            if inboxes.get(&id).is_some_and(|inbox| inbox.epoch == epoch) && rows.contains_key(&id)
            {
                if let Some(previous) = previous {
                    rows.insert(id.clone(), previous);
                }
            }
            // Remove only the failed epoch's work; a later user message remains independent.
            if let Some(inbox) = inboxes.get_mut(&id) {
                inbox
                    .messages
                    .retain(|(_, queued_id, _)| *queued_id != message_id);
            }
        }
        result
    }

    pub async fn begin_human_task_resume(&self, id: &str, epoch: u64) -> Result<(), TaskError> {
        let id = self.canonical_or_raw(id).await;
        let mut rows = self.tasks.write().await;
        let inboxes = self.human_messages.lock().unwrap();
        let inbox = inboxes.get(&id).ok_or(TaskError::TerminatedTask)?;
        if inbox.epoch != epoch || inbox.cancel.is_cancelled() {
            return Err(TaskError::TerminatedTask);
        }
        let Some(TaskState::LocalAgent(agent)) = rows.get_mut(&id) else {
            return Err(TaskError::NotFound(id));
        };
        agent.base.status = TaskStatus::Running;
        agent.base.end_time = None;
        agent.base.evict_after = None;
        agent.base.notified = false;
        agent.is_parked = false;
        agent.is_backgrounded = true;
        agent.outcome.killed_by = None;
        agent.outcome.max_turns_reached = None;
        agent.error = None;
        Ok(())
    }

    pub async fn take_human_task_messages_for(
        &self,
        agent_id: lingxi_core::types::AgentId,
    ) -> Vec<String> {
        let rows = self.tasks.read().await;
        let Some((id, _)) = rows.iter().find(|(_, state)| matches!(state, TaskState::LocalAgent(agent) if agent.agent_id == agent_id && !state.is_terminated())) else { return Vec::new(); };
        let mut inboxes = self.human_messages.lock().unwrap();
        let Some(inbox) = inboxes.get_mut(id) else {
            return Vec::new();
        };
        let epoch = inbox.epoch;
        inbox
            .messages
            .drain(..)
            .filter_map(|(queued, _, text)| (queued == epoch).then_some(text))
            .collect()
    }

    pub async fn register_agent_resume_recipe(
        &self,
        id: &str,
        request: lingxi_core::host::SubagentSpawnRequest,
        inheritance: lingxi_core::host::SubagentInheritance,
    ) -> Result<(), TaskError> {
        let id = self.canonical_or_raw(id).await;
        self.handler_for(TaskType::LocalAgent)
            .await
            .ok_or(TaskError::Unsupported)?
            .register_resume_recipe(&id, request, inheritance)
            .await
    }
}
