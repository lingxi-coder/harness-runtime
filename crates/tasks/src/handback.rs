//! A report succeeds when its recipient queue and receipt have both accepted
//! it. Consumption happens later, outside the synchronous caller's turn gate.
use super::*;
use lingxi_core::host::handback::{
    BeginHandbackRun, HandbackAdmissionError, HandbackAdmissionOutcome, HandbackDisposition,
    HandbackEnvelope, HandbackReceipt, HandbackRecipient, HandbackRunKey, HandbackRunToken,
    HandbackSessionScope, HandbackState, PreparedHandbackReport, ReportingAdmission,
};
use lingxi_core::types::AgentId;

/// Recipient-owned journal. A queued report is durable before its sender gets
/// an admission receipt; a wakeup is merely a hint to consume this queue.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PeerInbox {
    version: u32,
    scope: HandbackSessionScope,
    recipient: AgentId,
    pending: Vec<HandbackEnvelope>,
    consumed: Vec<HandbackReceipt>,
}

impl PeerInbox {
    fn empty(scope: HandbackSessionScope, recipient: AgentId) -> Self {
        Self {
            version: 1,
            scope,
            recipient,
            pending: Vec::new(),
            consumed: Vec::new(),
        }
    }

    fn valid_for(&self, scope: HandbackSessionScope, recipient: AgentId) -> bool {
        self.version == 1 && self.scope.session_id == scope.session_id && self.recipient == recipient
            && self.pending.iter().all(|report| report.validate() && report.receipt.run.scope.session_id == scope.session_id && matches!(report.receipt.recipient, HandbackRecipient::Agent { agent_id, .. } if agent_id == recipient))
            && self.consumed.iter().all(|receipt| receipt.run.scope.session_id == scope.session_id && receipt.run.scope == receipt.recipient.scope() && matches!(receipt.recipient, HandbackRecipient::Agent { agent_id, .. } if agent_id == recipient))
    }
}

fn actor(state: &TaskState) -> Option<AgentId> {
    match state {
        TaskState::LocalAgent(agent) => Some(agent.agent_id),
        TaskState::InProcessTeammate(agent) => Some(agent.agent_id),
        _ => None,
    }
}

impl TaskRegistry {
    fn peer_inbox_path(scope: HandbackSessionScope, recipient: AgentId) -> std::path::PathBuf {
        format!(
            "{}-{}.handback-inbox.json",
            scope.session_id.as_uuid().simple(),
            recipient.as_uuid().simple()
        )
        .into()
    }

    async fn persist_peer_inbox(&self, inbox: &PeerInbox) -> Result<(), TaskError> {
        let json =
            serde_json::to_string(inbox).map_err(|error| TaskError::Internal(error.to_string()))?;
        self.fs
            .write_file_rooted_atomic(
                self.output_manager.output_dir(),
                &Self::peer_inbox_path(inbox.scope, inbox.recipient),
                &json,
            )
            .await
            .map_err(|error| TaskError::Io(error.to_string()))
    }

    /// Only an intentional cold restoration may import an older activation's
    /// already admitted inbox. External tool submissions still require the
    /// exact live scope and run token. Stored receipts are never rewritten.
    async fn restore_peer_inbox(
        &self,
        recipient: AgentId,
        scope: HandbackSessionScope,
    ) -> Result<(), TaskError> {
        if self
            .peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&recipient)
            .is_some_and(|inbox| inbox.scope == scope)
        {
            return Ok(());
        }
        let file = match self
            .fs
            .read_file_rooted_no_follow(
                self.output_manager.output_dir(),
                &Self::peer_inbox_path(scope, recipient),
            )
            .await
        {
            Ok(file) => file,
            Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(TaskError::Io(error.to_string())),
        };
        if file.content.is_empty() {
            return Ok(());
        }
        let mut inbox: PeerInbox = serde_json::from_str(&file.content).map_err(|error| {
            TaskError::Internal(format!("invalid retained peer inbox: {error}"))
        })?;
        if !inbox.valid_for(scope, recipient) {
            return Err(TaskError::Internal(
                "retained peer inbox has an invalid recipient or origin".into(),
            ));
        }
        inbox.scope = scope;
        // Rebinding the journal's owned activation does not retarget any
        // admitted receipt; it permits this explicit recovery to consume it.
        self.persist_peer_inbox(&inbox).await?;
        self.peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(recipient, inbox);
        Ok(())
    }

    pub async fn pending_handback_reports_for(&self, agent_id: AgentId) -> Vec<HandbackEnvelope> {
        let Some(scope) = self.handback_scope().await else {
            return Vec::new();
        };
        if !self.tasks.read().await.values().any(|row| {
            actor(row) == Some(agent_id)
                && !row.is_terminated()
                && !matches!(row, TaskState::LocalAgent(agent) if agent.outcome.killed_by.is_some())
        }) {
            return Vec::new();
        }
        self.peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agent_id)
            .filter(|inbox| inbox.scope == scope)
            .map(|inbox| inbox.pending.clone())
            .unwrap_or_default()
    }

    pub async fn acknowledge_handback_consumption(
        &self,
        agent_id: AgentId,
        receipt: &HandbackReceipt,
    ) -> bool {
        if let Some(registry) = self.owned_self() {
            let receipt = receipt.clone();
            return tokio::spawn(async move {
                registry
                    .acknowledge_handback_consumption_inner(agent_id, &receipt)
                    .await
            })
            .await
            .unwrap_or(false);
        }
        self.acknowledge_handback_consumption_inner(agent_id, receipt)
            .await
    }

    async fn acknowledge_handback_consumption_inner(
        &self,
        agent_id: AgentId,
        receipt: &HandbackReceipt,
    ) -> bool {
        let _transaction = self.handback_transactions.lock().await;
        let Some(scope) = self.handback_scope().await else {
            return false;
        };
        let mut inbox = match self
            .peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agent_id)
            .cloned()
        {
            Some(inbox) if inbox.scope == scope => inbox,
            _ => return false,
        };
        if inbox.consumed.contains(receipt) {
            return true;
        }
        let Some(index) = inbox
            .pending
            .iter()
            .position(|pending| &pending.receipt == receipt)
        else {
            return false;
        };
        inbox.pending.remove(index);
        inbox.consumed.push(receipt.clone());
        if self.persist_peer_inbox(&inbox).await.is_err() {
            return false;
        }
        self.peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent_id, inbox);
        true
    }

    async fn admit_peer_inbox(&self, envelope: HandbackEnvelope) -> Result<(), TaskError> {
        let HandbackRecipient::Agent { scope, agent_id } = envelope.receipt.recipient else {
            return Err(TaskError::Unsupported);
        };
        if !envelope.validate() {
            return Err(TaskError::Internal("invalid peer report envelope".into()));
        }
        let mut inbox = self
            .peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agent_id)
            .filter(|inbox| inbox.scope == scope)
            .cloned()
            .unwrap_or_else(|| PeerInbox::empty(scope, agent_id));
        if inbox.consumed.contains(&envelope.receipt)
            || inbox
                .pending
                .iter()
                .any(|pending| pending.receipt == envelope.receipt)
        {
            return Ok(());
        }
        inbox.pending.push(envelope);
        self.persist_peer_inbox(&inbox).await?;
        self.peer_inboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent_id, inbox);
        self.bump_notification_revision();
        Ok(())
    }
    /// Bind the independently owned root without retaining its reverse link
    /// to this registry. Each operation holds a temporary live owner through
    /// its awaited scope/admission transaction.
    pub fn bind_reporting_admission(
        &self,
        admission: std::sync::Weak<dyn ReportingAdmission>,
    ) -> bool {
        self.reporting_admission.set(admission).is_ok()
    }

    pub async fn handback_scope(&self) -> Option<HandbackSessionScope> {
        let admission = self.reporting_admission.get()?.upgrade()?;
        admission.main_scope().await
    }

    fn live_handback_token(&self, token: &HandbackRunToken) -> bool {
        self.handback_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&token.run().agent_id)
            .is_some_and(|current| {
                current.run() == token.run() && current.registration_id() == token.registration_id()
            })
    }

    fn readable_handback_recipient(
        &self,
        rows: &HashMap<String, TaskState>,
        recipient: &HandbackRecipient,
        scope: &HandbackSessionScope,
    ) -> bool {
        match recipient {
            HandbackRecipient::Main {
                scope: recipient_scope,
            } => recipient_scope == scope,
            HandbackRecipient::Agent {
                scope: recipient_scope,
                agent_id,
            } => {
                recipient_scope == scope
                    && rows.values().any(|state| {
                        if actor(state) != Some(*agent_id)
                            || state
                                .handback()
                                .is_some_and(|state| &state.run.scope != scope)
                        {
                            return false;
                        }
                        match state {
                            TaskState::LocalAgent(agent) => {
                                agent.outcome.killed_by.is_none()
                                    && (agent.base.status == TaskStatus::Running
                                        || (agent.base.status == TaskStatus::Completed
                                            && self.has_live_background_children_locked(
                                                rows,
                                                Some(*agent_id),
                                                None,
                                                None,
                                            )))
                            }
                            _ => false,
                        }
                    })
            }
        }
    }

    pub async fn begin_handback_run(
        &self,
        input: BeginHandbackRun,
    ) -> Result<HandbackRunToken, TaskError> {
        if let Some(registry) = self.owned_self() {
            return tokio::spawn(async move { registry.begin_handback_run_inner(input).await })
                .await
                .map_err(|error| {
                    TaskError::Internal(format!("handback startup transaction joined: {error}"))
                })?;
        }
        self.begin_handback_run_inner(input).await
    }

    async fn begin_handback_run_inner(
        &self,
        input: BeginHandbackRun,
    ) -> Result<HandbackRunToken, TaskError> {
        let _transaction = self.handback_transactions.lock().await;
        let _lifecycle = self.lifecycle_gate.read().await;
        if !self.accepting_tasks.load(Ordering::Acquire) {
            return Err(TaskError::TerminatedTask);
        }
        if input.active && self.handback_scope().await.as_ref() != Some(&input.scope) {
            return Err(TaskError::Internal(
                "handback session activation is unavailable".into(),
            ));
        }
        let restoring = input.restored_state.is_some();
        if restoring {
            self.restore_peer_inbox(input.agent_id, input.scope).await?;
        }
        if input
            .restored_state
            .as_ref()
            .is_some_and(|state| state.run.agent_id != input.agent_id)
        {
            return Err(TaskError::Internal(
                "restored handback actor does not match".into(),
            ));
        }
        let mut rows = self.tasks.write().await;
        let task_id = rows
            .iter()
            .find_map(|(id, state)| (actor(state) == Some(input.agent_id)).then(|| id.clone()))
            .ok_or_else(|| TaskError::NotFound(input.agent_id.to_string()))?;
        let actor_row = rows.get(&task_id).ok_or(TaskError::TerminatedTask)?;
        if actor_row.is_terminated()
            || matches!(actor_row, TaskState::LocalAgent(agent) if agent.outcome.killed_by.is_some())
            || lingxi_core::host::agent_processes::is_stop_pending(&input.agent_id.to_string())
        {
            return Err(TaskError::TerminatedTask);
        }
        if input.active && !matches!(actor_row, TaskState::LocalAgent(_)) {
            return Err(TaskError::Unsupported);
        }
        let mut previous = rows
            .get(&task_id)
            .and_then(TaskState::handback)
            .cloned()
            .or(input.restored_state);
        let archived_previous = previous.clone();
        if restoring {
            if let Some(previous) = &mut previous {
                if previous.recipient.scope().session_id == input.scope.session_id {
                    let rebound = match previous.recipient {
                        HandbackRecipient::Main { .. } => {
                            HandbackRecipient::Main { scope: input.scope }
                        }
                        HandbackRecipient::Agent { agent_id, .. } => HandbackRecipient::Agent {
                            scope: input.scope,
                            agent_id,
                        },
                    };
                    if self.readable_handback_recipient(&rows, &rebound, &input.scope) {
                        // Only the mutable owner is rebound; the old admitted
                        // receipt and archived report remain immutable.
                        previous.recipient = rebound;
                    }
                }
            }
        }
        let previous_epoch = previous.as_ref().map_or(0, |state| state.run.run_epoch);
        let run_epoch = previous_epoch
            .checked_add(1)
            .ok_or_else(|| TaskError::Internal("handback run epoch exhausted".into()))?;
        let recipient = if !input.active {
            previous
                .as_ref()
                .map(|state| state.recipient)
                .unwrap_or(input.resumer)
        } else if let Some(previous) = &previous {
            if self.readable_handback_recipient(&rows, &previous.recipient, &input.scope) {
                previous.recipient
            } else {
                input.resumer
            }
        } else {
            input
                .caller
                .filter(|caller| self.readable_handback_recipient(&rows, caller, &input.scope))
                .unwrap_or(input.resumer)
        };
        let run = HandbackRunKey {
            scope: input.scope,
            agent_id: input.agent_id,
            run_epoch,
        };
        let token = HandbackRunToken::mint(run);
        let state = rows.get_mut(&task_id).ok_or(TaskError::TerminatedTask)?;
        for archived in input.restored_history {
            if archived.run.agent_id == input.agent_id
                && !state
                    .handback_history()
                    .iter()
                    .any(|old| old.run == archived.run && old.receipt == archived.receipt)
            {
                state.archive_handback(archived);
            }
        }
        if let Some(previous) = archived_previous {
            state.archive_handback(previous);
        }
        *state.handback_slot().ok_or(TaskError::Unsupported)? = Some(HandbackState {
            active: input.active,
            run,
            recipient,
            fallback_main: input.scope,
            bounce_count: 0,
            receipt: None,
            report: None,
            disposition: None,
        });
        if let TaskState::LocalAgent(agent) = state {
            agent.outcome.reset_run();
            agent.error = None;
        }
        drop(rows);
        self.pending_rest.write().await.remove(&task_id);
        self.handback_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(input.agent_id, token.clone());
        Ok(token)
    }

    pub async fn handback_state(&self, token: &HandbackRunToken) -> Option<HandbackState> {
        if !self.live_handback_token(token) {
            return None;
        }
        self.handback_state_for_agent(token.run().agent_id)
            .await
            .filter(|state| state.run == token.run())
    }

    pub async fn handback_state_for_agent(&self, agent_id: AgentId) -> Option<HandbackState> {
        self.tasks
            .read()
            .await
            .values()
            .find(|state| actor(state) == Some(agent_id))
            .and_then(TaskState::handback)
            .cloned()
    }

    pub async fn handback_history_for_agent(&self, agent_id: AgentId) -> Vec<HandbackState> {
        self.tasks
            .read()
            .await
            .values()
            .find(|state| actor(state) == Some(agent_id))
            .map(|state| state.handback_history().to_vec())
            .unwrap_or_default()
    }

    pub async fn agent_waiting_on_owned_work(&self, agent_id: AgentId) -> bool {
        let rows = self.tasks.read().await;
        self.has_live_background_children_locked(&rows, Some(agent_id), None, None)
    }

    pub async fn next_handback_bounce(&self, token: &HandbackRunToken) -> Option<u8> {
        let _transaction = self.handback_transactions.lock().await;
        if !self.live_handback_token(token) {
            return None;
        }
        let mut rows = self.tasks.write().await;
        let state = rows
            .values_mut()
            .find(|state| actor(state) == Some(token.run().agent_id))?
            .handback_slot()?
            .as_mut()?;
        if state.run != token.run() {
            return None;
        }
        state.next_bounce(false, false)
    }

    pub async fn set_handback_disposition(
        &self,
        token: &HandbackRunToken,
        disposition: HandbackDisposition,
    ) {
        let _transaction = self.handback_transactions.lock().await;
        if !self.live_handback_token(token) {
            return;
        }
        let mut rows = self.tasks.write().await;
        if let Some(state) = rows
            .values_mut()
            .find(|state| actor(state) == Some(token.run().agent_id))
            .and_then(TaskState::handback_slot)
            .and_then(Option::as_mut)
        {
            if state.receipt.is_none() || disposition != HandbackDisposition::Withheld {
                state.disposition = Some(disposition);
            }
        }
    }

    pub async fn try_deliver_handback(
        &self,
        token: &HandbackRunToken,
        report: PreparedHandbackReport,
    ) -> HandbackAdmissionOutcome {
        if let Some(registry) = self.owned_self() {
            let token = token.clone();
            return tokio::spawn(async move {
                registry.try_deliver_handback_inner(&token, report).await
            })
            .await
            .unwrap_or(HandbackAdmissionOutcome::Rejected);
        }
        self.try_deliver_handback_inner(token, report).await
    }

    async fn try_deliver_handback_inner(
        &self,
        token: &HandbackRunToken,
        mut report: PreparedHandbackReport,
    ) -> HandbackAdmissionOutcome {
        let _transaction = self.handback_transactions.lock().await;
        let _admission = self.lifecycle_gate.read().await;
        if !self.accepting_tasks.load(Ordering::Acquire) || !self.live_handback_token(token) {
            return HandbackAdmissionOutcome::StaleRun;
        }
        let Some(admission) = self
            .reporting_admission
            .get()
            .and_then(std::sync::Weak::upgrade)
        else {
            return HandbackAdmissionOutcome::Rejected;
        };
        if admission.main_scope().await.as_ref() != Some(&token.run().scope) {
            return HandbackAdmissionOutcome::StaleRun;
        }
        let (task_id, backgrounded, mut recipient) = {
            let rows = self.tasks.read().await;
            let Some((task_id, sender)) = rows
                .iter()
                .find(|(_, state)| actor(state) == Some(token.run().agent_id))
            else {
                return HandbackAdmissionOutcome::StaleRun;
            };
            let Some(state) = sender.handback() else {
                return HandbackAdmissionOutcome::Inactive;
            };
            if state.receipt.is_some() {
                return HandbackAdmissionOutcome::Duplicate;
            }
            if !state.active {
                return HandbackAdmissionOutcome::Inactive;
            }
            if sender.is_terminated()
                || matches!(sender, TaskState::LocalAgent(agent) if agent.outcome.killed_by.is_some())
            {
                return HandbackAdmissionOutcome::StaleRun;
            }
            let backgrounded = match sender {
                TaskState::LocalAgent(agent) => agent.is_backgrounded,
                TaskState::InProcessTeammate(_) => true,
                _ => false,
            };
            let recipient =
                if self.readable_handback_recipient(&rows, &state.recipient, &state.run.scope) {
                    state.recipient
                } else if backgrounded {
                    HandbackRecipient::Main {
                        scope: state.fallback_main,
                    }
                } else {
                    return HandbackAdmissionOutcome::CallerGone;
                };
            (task_id.clone(), backgrounded, recipient)
        };
        // The queue never trusts a model-supplied task id. Only this registry's
        // canonical sender identity enters the peer origin.
        report.sender_task_id = token.run().agent_id.to_string();
        let mut envelope = report.envelope(token.run(), recipient);
        if !envelope.validate() {
            return HandbackAdmissionOutcome::Rejected;
        }
        let accepted = match &recipient {
            HandbackRecipient::Main { .. } => admission.admit(envelope.clone()).await,
            HandbackRecipient::Agent { agent_id, .. } => {
                let target =
                    self.tasks.read().await.iter().find_map(|(id, state)| {
                        (actor(state) == Some(*agent_id)).then(|| id.clone())
                    });
                match target {
                    Some(target) => {
                        match self.admit_peer_inbox(envelope.clone()).await {
                            Ok(()) => {
                                // Wakeups do not participate in admission. A
                                // full event channel cannot deadlock a parent
                                // synchronously awaiting this child report.
                                if let Some(registry) = self.owned_self() {
                                    let wake = envelope.clone();
                                    tokio::spawn(async move {
                                        let _ = registry.send_peer_to_task(&target, wake).await;
                                    });
                                }
                                Ok(())
                            }
                            Err(error) => Err(HandbackAdmissionError::Rejected {
                                reason: error.to_string(),
                            }),
                        }
                    }
                    None => Err(HandbackAdmissionError::Unavailable),
                }
            }
        };
        let accepted = if accepted.is_err()
            && backgrounded
            && matches!(recipient, HandbackRecipient::Agent { .. })
        {
            recipient = HandbackRecipient::Main {
                scope: token.run().scope,
            };
            envelope = report.envelope(token.run(), recipient);
            admission.admit(envelope.clone()).await
        } else {
            accepted
        };
        // Fallback owner is mutable even when admission is rejected; birth
        // ancestry remains untouched and keeps governing recursive ownership.
        let mut rows = self.tasks.write().await;
        let Some(state) = rows
            .get_mut(&task_id)
            .and_then(TaskState::handback_slot)
            .and_then(Option::as_mut)
        else {
            return HandbackAdmissionOutcome::StaleRun;
        };
        state.recipient = recipient;
        if let Err(error) = accepted {
            return match error {
                HandbackAdmissionError::StaleScope => HandbackAdmissionOutcome::StaleRun,
                _ => HandbackAdmissionOutcome::Rejected,
            };
        }
        let receipt = envelope.receipt;
        state.receipt = Some(receipt.clone());
        state.report = Some(report.report);
        state.disposition = Some(if report.flagged {
            HandbackDisposition::Flagged
        } else {
            HandbackDisposition::Send
        });
        drop(rows);
        self.bump_notification_revision();
        HandbackAdmissionOutcome::Admitted(receipt)
    }

    /// Stops invalidate only current live authority; persisted sanitized
    /// reports and previous dispositions remain inspectable.
    pub(crate) async fn invalidate_handback_for_task(&self, task_id: &str) {
        let _transaction = self.handback_transactions.lock().await;
        let mut rows = self.tasks.write().await;
        let Some(state) = rows.get_mut(task_id) else {
            return;
        };
        if let Some(agent_id) = actor(state) {
            self.handback_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&agent_id);
        }
        if let Some(state) = state.handback_slot().and_then(Option::as_mut) {
            state.active = false;
        }
    }

    pub(super) async fn admit_peer_to_task(
        &self,
        task_id: &str,
        envelope: HandbackEnvelope,
    ) -> Result<(), TaskError> {
        if let Some(registry) = self.owned_self() {
            let task_id = task_id.to_owned();
            return tokio::spawn(async move {
                registry.admit_peer_to_task_inner(&task_id, envelope).await
            })
            .await
            .map_err(|error| {
                TaskError::Internal(format!("peer admission transaction joined: {error}"))
            })?;
        }
        self.admit_peer_to_task_inner(task_id, envelope).await
    }

    async fn admit_peer_to_task_inner(
        &self,
        task_id: &str,
        envelope: HandbackEnvelope,
    ) -> Result<(), TaskError> {
        let _transaction = self.handback_transactions.lock().await;
        let scope = self.handback_scope().await.ok_or(TaskError::Unsupported)?;
        let recipient = match envelope.receipt.recipient {
            HandbackRecipient::Agent {
                agent_id,
                scope: target_scope,
            } if target_scope == scope => agent_id,
            _ => return Err(TaskError::TerminatedTask),
        };
        if envelope.receipt.run.scope != scope || !envelope.validate() {
            return Err(TaskError::TerminatedTask);
        }
        if !self.tasks.read().await.get(task_id).is_some_and(|row| {
            actor(row) == Some(recipient)
                && !row.is_terminated()
                && !matches!(row, TaskState::LocalAgent(agent) if agent.outcome.killed_by.is_some())
        }) {
            return Err(TaskError::TerminatedTask);
        }
        self.admit_peer_inbox(envelope.clone()).await?;
        if let Some(registry) = self.owned_self() {
            let task_id = task_id.to_owned();
            tokio::spawn(async move {
                let _ = registry.send_peer_to_task(&task_id, envelope).await;
            });
        }
        Ok(())
    }

    pub(super) async fn send_peer_to_task(
        &self,
        task_id: &str,
        envelope: HandbackEnvelope,
    ) -> Result<(), TaskError> {
        if !envelope.validate() {
            return Err(TaskError::Internal("invalid peer report envelope".into()));
        }
        if matches!(self.tasks.read().await.get(task_id), Some(TaskState::LocalAgent(agent)) if agent.outcome.killed_by.as_deref() == Some("user"))
        {
            return Err(TaskError::TerminatedTask);
        }
        let receiver = self
            .agent_message_receivers
            .lock()
            .await
            .get(task_id)
            .cloned();
        if let Some(receiver) = receiver {
            return receiver
                .send_peer(envelope)
                .await
                .map_err(|error| TaskError::Internal(error.to_string()));
        }
        let task_type = self
            .spawned
            .read()
            .await
            .get(task_id)
            .copied()
            .ok_or(TaskError::TerminatedTask)?;
        let handler = self
            .handlers
            .read()
            .await
            .get(&task_type)
            .cloned()
            .ok_or(TaskError::Unsupported)?;
        handler
            .send_peer(
                task_id,
                envelope,
                TaskContext {
                    fs: self.fs.clone(),
                    runtime: self.runtime.clone(),
                },
            )
            .await
    }
}
