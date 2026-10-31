//! Host-only recovery authority. A prepared generation fences admission; only
//! the final ledger rename makes its projection authoritative. Old task/event
//! records and attachment references are never rewritten by this transaction.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatRecoveryFrontier {
    app_id: String,
    chat_id: String,
    generation: String,
    source_generation: Option<String>,
    base_task_id: Option<String>,
    checkpoint: Option<ChatTranscriptCheckpoint>,
    abandoned_run_ids: BTreeSet<String>,
}

impl ChatRecoveryFrontier {
    pub fn base_task_id(&self) -> Option<&str> {
        self.base_task_id.as_deref()
    }
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn source_generation(&self) -> Option<&str> {
        self.source_generation.as_deref()
    }
    pub fn checkpoint(&self) -> Option<&ChatTranscriptCheckpoint> {
        self.checkpoint.as_ref()
    }
    pub fn abandoned_run_ids(&self) -> &BTreeSet<String> {
        &self.abandoned_run_ids
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ChatRecoveryRecord {
    pub(super) active: Option<ChatRecoveryFrontier>,
    pub(super) pending: Option<ChatRecoveryFrontier>,
}

fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}

pub(super) fn archived(ledger: &Ledger, chat_id: &str, run_id: &str) -> bool {
    ledger
        .chat_recoveries
        .get(chat_id)
        .and_then(|record| record.active.as_ref())
        .is_some_and(|frontier| frontier.abandoned_run_ids.contains(run_id))
}

pub(super) fn pending(ledger: &Ledger, chat_id: &str) -> bool {
    ledger
        .chat_recoveries
        .get(chat_id)
        .is_some_and(|record| record.pending.is_some())
}

fn validate_frontier(ledger: &Ledger, frontier: &ChatRecoveryFrontier) -> Result<(), AppError> {
    if !valid_app_id(&frontier.app_id)
        || !canonical_uuid(&frontier.chat_id)
        || !canonical_uuid(&frontier.generation)
        || frontier
            .source_generation
            .as_deref()
            .is_some_and(|id| !canonical_uuid(id))
        || frontier.abandoned_run_ids.is_empty()
    {
        return Err(AgentStore::storage_unavailable());
    }
    match (&frontier.base_task_id, &frontier.checkpoint) {
        (Some(id), Some(checkpoint)) => {
            let record = ledger
                .runs
                .get(id)
                .ok_or_else(AgentStore::storage_unavailable)?;
            if record.owner.app_id != frontier.app_id
                || record.chat_id.as_deref() != Some(frontier.chat_id.as_str())
                || record.task.state != State::Succeeded
                || record.chat_transcript_checkpoint.as_ref() != Some(checkpoint)
                || checkpoint.byte_len == 0
                || !valid_hex(&checkpoint.sha256, 64)
            {
                return Err(AgentStore::storage_unavailable());
            }
        }
        (None, None) => {}
        _ => return Err(AgentStore::storage_unavailable()),
    }
    for id in &frontier.abandoned_run_ids {
        let record = ledger
            .runs
            .get(id)
            .ok_or_else(AgentStore::storage_unavailable)?;
        if record.owner.app_id != frontier.app_id
            || record.chat_id.as_deref() != Some(frontier.chat_id.as_str())
            || !record.task.state.terminal()
            || record.task.state == State::Succeeded
            || record.protocol_commit_pending.is_some()
        {
            return Err(AgentStore::storage_unavailable());
        }
    }
    Ok(())
}

pub(super) fn validate_records(ledger: &Ledger) -> Result<(), AppError> {
    for (chat_id, record) in &ledger.chat_recoveries {
        if record.active.is_none() && record.pending.is_none() {
            return Err(AgentStore::storage_unavailable());
        }
        for frontier in record.active.iter().chain(record.pending.iter()) {
            if frontier.chat_id != *chat_id
                || frontier.source_generation.as_deref() == Some(frontier.generation.as_str())
            {
                return Err(AgentStore::storage_unavailable());
            }
            validate_frontier(ledger, frontier)?;
        }
        if let Some(pending) = &record.pending {
            if pending.source_generation.as_deref()
                != record
                    .active
                    .as_ref()
                    .map(|active| active.generation.as_str())
                || record.active.as_ref().is_some_and(|active| {
                    !active
                        .abandoned_run_ids
                        .is_subset(&pending.abandoned_run_ids)
                })
            {
                return Err(AgentStore::storage_unavailable());
            }
        }
    }
    Ok(())
}

impl AgentStore {
    pub fn archived_chat_attempts(
        &self,
        owner: &Owner,
        chat_id: &str,
    ) -> Result<Vec<Task>, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let ids = ledger
            .chat_recoveries
            .get(chat_id)
            .and_then(|record| record.active.as_ref())
            .filter(|frontier| frontier.app_id == owner.app_id)
            .map(|frontier| &frontier.abandoned_run_ids);
        Ok(ids
            .into_iter()
            .flatten()
            .filter_map(|id| ledger.runs.get(id))
            .map(|record| record.task.clone())
            .collect())
    }

    pub fn chat_requires_recovery(&self, app_id: &str, chat_id: &str) -> Result<bool, AppError> {
        let ledger = self.lock_ledger()?;
        Ok(pending(&ledger, chat_id)
            || ledger.runs.values().any(|record| {
                record.owner.app_id == app_id
                    && record.chat_id.as_deref() == Some(chat_id)
                    && !archived(&ledger, chat_id, &record.task.run_id)
                    && record.task.state.terminal()
                    && record.task.state != State::Succeeded
            }))
    }

    /// Recover only host-selected history. No browser-provided checkpoint,
    /// transcript path or claimed task outcome is accepted by this API.
    pub fn prepare_chat_recovery(
        &self,
        owner: &Owner,
        chat_id: &str,
    ) -> Result<ChatRecoveryFrontier, AppError> {
        owner.validate()?;
        if !canonical_uuid(chat_id) {
            return Err(invalid("invalid Chat recovery selector"));
        }
        self.with_protocol_commit(|| {
            // Validate the old successful authority before preparing anything.
            let latest = self.latest_recoverable_chat_checkpoint(&owner.app_id, chat_id)?;
            self.change(|ledger| {
                if !ledger
                    .grants
                    .get(&owner.app_id)
                    .is_some_and(|grant| grant.owner.release_digest == owner.release_digest)
                {
                    return Err(invalid("Agent is not authorized for this release"));
                }
                if let Some(plan) = ledger
                    .chat_recoveries
                    .get(chat_id)
                    .and_then(|record| record.pending.as_ref())
                {
                    if plan.app_id != owner.app_id {
                        return Err(Self::storage_unavailable());
                    }
                    validate_frontier(ledger, plan)?;
                    return Ok(plan.clone());
                }
                let previous = ledger
                    .chat_recoveries
                    .get(chat_id)
                    .and_then(|record| record.active.as_ref())
                    .cloned();
                let mut abandoned = previous
                    .as_ref()
                    .map(|frontier| frontier.abandoned_run_ids.clone())
                    .unwrap_or_default();
                for record in ledger.runs.values().filter(|record| {
                    record.owner.app_id == owner.app_id
                        && record.chat_id.as_deref() == Some(chat_id)
                }) {
                    if !record.task.state.terminal() || record.protocol_commit_pending.is_some() {
                        return Err(AppError::Busy(chat_id.into()));
                    }
                    if record.task.state != State::Succeeded {
                        abandoned.insert(record.task.run_id.clone());
                    }
                }
                if abandoned.is_empty() {
                    return Err(invalid("Chat has no interrupted attempts"));
                }
                let plan = ChatRecoveryFrontier {
                    app_id: owner.app_id.clone(),
                    chat_id: chat_id.into(),
                    generation: uuid::Uuid::new_v4().to_string(),
                    source_generation: previous
                        .as_ref()
                        .map(|frontier| frontier.generation.clone()),
                    base_task_id: latest.as_ref().map(|(task, _)| task.run_id.clone()),
                    checkpoint: latest.as_ref().map(|(_, checkpoint)| checkpoint.clone()),
                    abandoned_run_ids: abandoned,
                };
                ledger
                    .chat_recoveries
                    .entry(chat_id.into())
                    .or_default()
                    .pending = Some(plan.clone());
                Ok(plan)
            })
        })?
    }

    fn latest_recoverable_chat_checkpoint(
        &self,
        app_id: &str,
        chat_id: &str,
    ) -> Result<Option<(Task, ChatTranscriptCheckpoint)>, AppError> {
        let id = {
            let ledger = self.lock_ledger()?;
            let mut candidates = ledger
                .runs
                .values()
                .filter(|record| {
                    record.owner.app_id == app_id
                        && record.chat_id.as_deref() == Some(chat_id)
                        && record.task.state == State::Succeeded
                })
                .map(|record| {
                    let checkpoint = record
                        .chat_transcript_checkpoint
                        .as_ref()
                        .ok_or_else(Self::storage_unavailable)?;
                    Ok((checkpoint.byte_len, record.task.run_id.clone()))
                })
                .collect::<Result<Vec<_>, AppError>>()?;
            candidates.sort();
            if candidates.len() > 1
                && candidates[candidates.len() - 1].0 == candidates[candidates.len() - 2].0
            {
                return Err(Self::storage_unavailable());
            }
            candidates.pop().map(|(_, id)| id)
        };
        id.map(|id| self.successful_chat_history_task(app_id, chat_id, &id))
            .transpose()
    }

    /// Caller must have verified and synchronized the exact staged generation
    /// and all retained canonical assets before this single authority switch.
    pub fn commit_chat_recovery(&self, plan: &ChatRecoveryFrontier) -> Result<(), AppError> {
        self.with_protocol_commit(|| {
            self.change(|ledger| {
                validate_frontier(ledger, plan)?;
                let record = ledger
                    .chat_recoveries
                    .get_mut(&plan.chat_id)
                    .ok_or_else(Self::storage_unavailable)?;
                if record.pending.as_ref() != Some(plan) {
                    return if record.pending.is_none() && record.active.as_ref() == Some(plan) {
                        Ok(())
                    } else {
                        Err(Self::storage_unavailable())
                    };
                }
                record.active = record.pending.take();
                Ok(())
            })
        })?
    }

    /// Rebuild the canonical projection from the one durable authority after
    /// reopening. A pending generation never replaces the active frontier.
    pub fn committed_chat_recoveries(
        &self,
        app_id: &str,
    ) -> Result<BTreeMap<String, ChatRecoveryFrontier>, AppError> {
        let ledger = self.lock_ledger()?;
        ledger
            .chat_recoveries
            .iter()
            .filter_map(|(id, record)| record.active.as_ref().map(|frontier| (id, frontier)))
            .map(|(id, frontier)| {
                validate_frontier(&ledger, frontier)?;
                if frontier.app_id != app_id
                    || frontier.chat_id != *id
                    || !canonical_uuid(id)
                    || !canonical_uuid(&frontier.generation)
                {
                    return Err(Self::storage_unavailable());
                }
                for run_id in &frontier.abandoned_run_ids {
                    let task = ledger
                        .runs
                        .get(run_id)
                        .ok_or_else(Self::storage_unavailable)?;
                    if task.owner.app_id != app_id
                        || task.chat_id.as_deref() != Some(id.as_str())
                        || !task.task.state.terminal()
                        || task.task.state == State::Succeeded
                    {
                        return Err(Self::storage_unavailable());
                    }
                }
                Ok((id.clone(), frontier.clone()))
            })
            .collect()
    }
}
