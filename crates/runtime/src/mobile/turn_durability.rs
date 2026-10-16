//! Crash-safe durable metadata for ordinary mobile conversation turns.
//!
//! This store deliberately does not claim arbitrary tool replay is exactly
//! once. It persists the user input, ordered client events, and a conservative
//! recovery gate. A caller may resume automatically only while
//! `safe_to_resume` is true; permission-bound or side-effectful work must park
//! as `WaitingForUser`.

use client::protocol::commands::{ImageRefDto, PromptModeDto};
use client::protocol::events::{TurnRecoverySnapshotDto, TurnRecoveryStateDto};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ImageSource, MessageId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const MAX_RETAINED_EVENTS: usize = 8_192;
const JOURNAL_COMPACTION_SLACK: usize = 256;

/// The exact source projections needed to re-create a native prompt-skill
/// batch after a process restart. The batch itself is rebuilt through the
/// orchestrator's one source-aware factory; no row is flattened into a String.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DurableSkillPrompt {
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub(crate) command: Utf16JsonProjection,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub(crate) body: Utf16JsonProjection,
    #[serde(with = "source_projection_vec_serde")]
    pub(crate) hook_messages: Vec<Utf16JsonProjection>,
    /// Stable identities for the command and expanded-body user rows.
    pub(crate) input_message_ids: [MessageId; 2],
    /// Stable identities for raw hook rows, in `hook_messages` order.
    pub(crate) hook_message_ids: Vec<MessageId>,
    /// Stable JSONL prompt IDs for the command and body rows.
    pub(crate) input_prompt_ids: [String; 2],
}

/// Stable transcript identity for one ordinary mobile prompt row. Text and
/// images remain on the turn checkpoint; this record pins the UUID and native
/// prompt ID used by the JSONL append-once path across process restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DurablePromptIdentity {
    pub(crate) message_id: MessageId,
    pub(crate) prompt_id: String,
}

/// Result captured after a directly executed user Bash command. It is persisted
/// before creating a model follow-up so restart can continue from the known
/// result without executing an uncertain command again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DurableBashResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) interrupted: bool,
}

fn mobile_bash_result_projection(result: &DurableBashResult) -> Utf16JsonProjection {
    if result.interrupted {
        return "[Request interrupted by user]".into();
    }
    format!(
        "<bash-stdout>{}</bash-stdout><bash-stderr>{}</bash-stderr>",
        result.stdout, result.stderr
    )
    .into()
}

impl DurablePromptIdentity {
    fn new() -> Self {
        Self {
            message_id: MessageId::new(),
            prompt_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) fn queued_prompt_input(
        &self,
        text: Utf16JsonProjection,
        images: &[ImageRefDto],
    ) -> orchestrator::QueuedPromptInput {
        orchestrator::QueuedPromptInput {
            text,
            images: images
                .iter()
                .map(|image| lingxi_core::types::ImageSource::Base64 {
                    media_type: image.media_type.clone(),
                    data: image.base64.clone(),
                })
                .collect(),
            message_id: Some(self.message_id),
            pending_prompt_identity: Some(orchestrator::PendingPromptIdentity {
                prompt_id: self.prompt_id.clone(),
                trailing_history_row_ids: Vec::new(),
            }),
            ..orchestrator::QueuedPromptInput::default()
        }
    }
}

impl DurableSkillPrompt {
    pub(crate) fn new(
        command: Utf16JsonProjection,
        body: Utf16JsonProjection,
        hook_messages: Vec<Utf16JsonProjection>,
    ) -> Self {
        Self {
            command,
            body,
            hook_message_ids: (0..hook_messages.len()).map(|_| MessageId::new()).collect(),
            hook_messages,
            input_message_ids: [MessageId::new(), MessageId::new()],
            input_prompt_ids: [
                uuid::Uuid::new_v4().to_string(),
                uuid::Uuid::new_v4().to_string(),
            ],
        }
    }

    pub(crate) fn queued_prompt_inputs(
        &self,
        images: &[ImageRefDto],
    ) -> Vec<orchestrator::QueuedPromptInput> {
        let mut inputs = orchestrator::skill_prompt_inputs(
            self.command.clone(),
            self.body.clone(),
            self.hook_messages.clone(),
        );
        if let Some(first) = inputs.first_mut() {
            first
                .images
                .extend(images.iter().map(|image| ImageSource::Base64 {
                    media_type: image.media_type.clone(),
                    data: image.base64.clone(),
                }));
        }
        for (((input, message_id), prompt_id), trailing_history_row_ids) in inputs
            .iter_mut()
            .zip(self.input_message_ids)
            .zip(&self.input_prompt_ids)
            .zip(self.trailing_history_ids_by_input())
        {
            input.message_id = Some(message_id);
            input.pending_prompt_identity = Some(orchestrator::PendingPromptIdentity {
                prompt_id: prompt_id.clone(),
                trailing_history_row_ids,
            });
        }
        inputs
    }

    fn trailing_history_ids_by_input(&self) -> Vec<Vec<MessageId>> {
        vec![Vec::new(), self.hook_message_ids.clone()]
    }
}

/// Serialize each history row as its source JSON value rather than as the
/// projection arena or a replacement-safe display string.
mod source_projection_vec_serde {
    use lingxi_core::types::utf16_json::{raw_json_serde, Utf16JsonProjection};
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::value::RawValue;

    struct RawProjection<'a>(&'a Utf16JsonProjection);

    impl Serialize for RawProjection<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            raw_json_serde::serialize(self.0, serializer)
        }
    }

    pub(super) fn serialize<S>(
        values: &[Utf16JsonProjection],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(values.len()))?;
        for value in values {
            sequence.serialize_element(&RawProjection(value))?;
        }
        sequence.end()
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Utf16JsonProjection>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<Box<RawValue>>::deserialize(deserializer)?
            .into_iter()
            .map(|raw| Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DurableTurnEvent {
    pub(crate) sequence: u64,
    pub(crate) event_json: String,
}

/// Private queue lifecycle. Pending prompts are never lowered to a public
/// `TurnRecoveryStateDto`; the `state` field stays `None` until a between-turn
/// worker claims the entry as a real model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum QueueDeliveryState {
    Pending,
    CoalescedPending,
    /// The joined row has crossed the in-turn delivery admission point. It is
    /// still cold-recoverable by its stable append identity, but cancellation
    /// must now unwind the owning active turn rather than withdraw one member.
    CoalescedClaimed,
    Claimed,
    /// A side-effecting queued slash operation was claimed before dispatch.
    /// It is deliberately excluded from cold queue reconstruction because a
    /// crash after the external effect is uncertain must never execute it twice.
    SlashClaimed,
    /// A queued Bash input crossed its at-most-once execution boundary.
    BashClaimed,
    Delivered,
    Cancelled,
}

/// Private operation discriminator for durable Mobile dispatch entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum QueueOperationKind {
    Prompt,
    Slash,
}

/// A burst consumed by the in-turn queue keeps one stable model/history row.
/// The first member owns the store commit marker; Native uses the last source
/// UUID for the aggregate row. All source identities remain ordered here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoalescedPromptMemberIdentity {
    pub(crate) turn_id: u64,
    pub(crate) original_client_turn_id: Option<u64>,
    pub(crate) prompt_identity: DurablePromptIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoalescedPromptBatch {
    pub(crate) owner_turn_id: u64,
    pub(crate) member_turn_ids: Vec<u64>,
    pub(crate) member_identities: Vec<CoalescedPromptMemberIdentity>,
    pub(crate) aggregate_identity: DurablePromptIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableTurnCheckpoint {
    pub(crate) session_id: String,
    pub(crate) turn_id: u64,
    pub(crate) prompt: Utf16JsonProjection,
    pub(crate) prompt_identity: Option<DurablePromptIdentity>,
    pub(crate) skill_prompt: Option<DurableSkillPrompt>,
    pub(crate) prompt_mode: Option<PromptModeDto>,
    pub(crate) images: Vec<ImageRefDto>,
    pub(crate) queue_sequence: u64,
    pub(crate) queue_operation: QueueOperationKind,
    pub(crate) bash_result: Option<DurableBashResult>,
    pub(crate) bash_result_row_id: Option<MessageId>,
    pub(crate) bash_notice_identity: Option<DurablePromptIdentity>,
    pub(crate) bash_should_query: Option<bool>,
    pub(crate) original_client_turn_id: Option<u64>,
    pub(crate) queue_delivery: QueueDeliveryState,
    pub(crate) coalesced_batch: Option<CoalescedPromptBatch>,
    pub(crate) revision: u64,
    pub(crate) state: Option<TurnRecoveryStateDto>,
    pub(crate) first_sequence: u64,
    pub(crate) last_sequence: u64,
    pub(crate) safe_to_resume: bool,
    pub(crate) reason: Option<String>,
    pub(crate) events: Vec<DurableTurnEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredTurnMetadata {
    session_id: String,
    turn_id: u64,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    prompt: Utf16JsonProjection,
    prompt_identity: Option<DurablePromptIdentity>,
    skill_prompt: Option<DurableSkillPrompt>,
    prompt_mode: Option<PromptModeDto>,
    images: Vec<ImageRefDto>,
    queue_sequence: u64,
    queue_operation: QueueOperationKind,
    bash_result: Option<DurableBashResult>,
    bash_result_row_id: Option<MessageId>,
    bash_notice_identity: Option<DurablePromptIdentity>,
    bash_should_query: Option<bool>,
    original_client_turn_id: Option<u64>,
    queue_delivery: QueueDeliveryState,
    coalesced_batch: Option<CoalescedPromptBatch>,
    #[serde(default)]
    revision: u64,
    state: Option<TurnRecoveryStateDto>,
    #[serde(default)]
    first_sequence: u64,
    #[serde(default)]
    last_sequence: u64,
    #[serde(default)]
    safe_to_resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    events: Vec<DurableTurnEvent>,
}

#[derive(Debug, Clone)]
struct CachedTurn {
    checkpoint: DurableTurnCheckpoint,
    journal_event_count: usize,
}

impl DurableTurnCheckpoint {
    pub(crate) fn queued_prompt_inputs(
        &self,
        store: &DurableTurnStore,
    ) -> Result<Vec<orchestrator::QueuedPromptInput>, DurableTurnStoreError> {
        if self.queue_operation != QueueOperationKind::Prompt {
            return Err(DurableTurnStoreError::Storage(
                "slash operation cannot be lowered as model prompt input".into(),
            ));
        }
        if self.prompt_mode == Some(PromptModeDto::Bash) {
            if self.queue_delivery != QueueDeliveryState::Claimed || self.state.is_none() {
                return Err(DurableTurnStoreError::Storage(
                    "Bash history is unavailable before its durable result claim".into(),
                ));
            }
            let result = self.bash_result.as_ref().ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "claimed Bash prompt is missing its durable execution result".into(),
                )
            })?;
            let should_query = self.bash_should_query.ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "claimed Bash prompt is missing its response setting".into(),
                )
            })?;
            let prompt_identity = self.prompt_identity.as_ref().ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "claimed Bash prompt is missing its stable input identity".into(),
                )
            })?;
            let result_row_id = self.bash_result_row_id.ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "claimed Bash prompt is missing its stable result identity".into(),
                )
            })?;
            let result_row = mobile_bash_result_projection(result);
            let skip_model_query = !should_query || result.interrupted;
            let mut inputs = Vec::with_capacity(2);
            if skip_model_query {
                let notice_identity = self.bash_notice_identity.as_ref().ok_or_else(|| {
                    DurableTurnStoreError::Storage(
                        "non-query Bash prompt is missing its durable notice identity".into(),
                    )
                })?;
                let mut notice = notice_identity.queued_prompt_input(
                    "<system-reminder>The command below was run directly in Claude Code, not sent to you as a request, and its output goes straight to the user. It's recorded here as context for later messages.</system-reminder>".into(),
                    &[],
                );
                notice.is_meta = true;
                notice.skip_model_query = true;
                inputs.push(notice);
            }
            inputs.push(orchestrator::QueuedPromptInput {
                text: self.prompt.clone(),
                prompt_ingress_kind: Some(orchestrator::PromptIngressKind::DirectBash),
                skip_model_query,
                images: self
                    .images
                    .iter()
                    .map(|image| ImageSource::Base64 {
                        media_type: image.media_type.clone(),
                        data: image.base64.clone(),
                    })
                    .collect(),
                message_id: Some(prompt_identity.message_id),
                pending_prompt_identity: Some(orchestrator::PendingPromptIdentity {
                    prompt_id: prompt_identity.prompt_id.clone(),
                    trailing_history_row_ids: vec![result_row_id],
                }),
                trailing_history_rows: vec![result_row],
                ..orchestrator::QueuedPromptInput::default()
            });
            return Ok(inputs);
        }
        let members = if let Some(batch) = self.coalesced_batch.as_ref() {
            batch
                .member_turn_ids
                .iter()
                .map(|turn_id| store.load(&self.session_id, *turn_id))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![self.clone()]
        };
        let identity = self
            .coalesced_batch
            .as_ref()
            .map(|batch| &batch.aggregate_identity)
            .or(self.prompt_identity.as_ref())
            .ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "queued prompt checkpoint is missing its stable row identity".into(),
                )
            })?;
        let prompts = members
            .iter()
            .map(|member| member.prompt.clone())
            .collect::<Vec<_>>();
        let mut images = Vec::new();
        for member in &members {
            images.extend(member.images.iter().cloned());
        }
        Ok(vec![identity.queued_prompt_input(
            join_source_prompts(&prompts),
            &images,
        )])
    }

    pub(crate) fn is_pending_mobile_bash(&self) -> bool {
        self.queue_operation == QueueOperationKind::Prompt
            && self.prompt_mode == Some(PromptModeDto::Bash)
            && self.state.is_none()
            && self.queue_delivery == QueueDeliveryState::Pending
    }

    pub(crate) fn snapshot(&self) -> TurnRecoverySnapshotDto {
        TurnRecoverySnapshotDto {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id,
            state: self
                .state
                .clone()
                .expect("private queued prompts do not have recovery snapshots"),
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            safe_to_resume: self.safe_to_resume,
            reason: self.reason.clone(),
        }
    }

    pub(crate) fn snapshot_with_reason(&self, reason: Option<String>) -> TurnRecoverySnapshotDto {
        TurnRecoverySnapshotDto {
            reason,
            ..self.snapshot()
        }
    }

    pub(crate) fn replay_after(&self, after_sequence: Option<u64>) -> &[DurableTurnEvent] {
        let after = after_sequence.unwrap_or(0);
        let index = self.events.partition_point(|event| event.sequence <= after);
        &self.events[index..]
    }

    pub(crate) fn has_replay_gap(&self, after_sequence: Option<u64>) -> bool {
        let after = after_sequence.unwrap_or(0);
        self.first_sequence != 0 && after.saturating_add(1) < self.first_sequence
    }

    fn can_resume_without_user(&self) -> bool {
        self.state.is_some()
            && self.queue_delivery == QueueDeliveryState::Claimed
            && self.safe_to_resume
            && self
                .events
                .iter()
                .all(|event| is_resume_safe_retained_event(&event.event_json))
    }

    fn stored_metadata(&self) -> StoredTurnMetadata {
        StoredTurnMetadata {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id,
            prompt: self.prompt.clone(),
            prompt_identity: self.prompt_identity.clone(),
            skill_prompt: self.skill_prompt.clone(),
            prompt_mode: self.prompt_mode,
            images: self.images.clone(),
            queue_sequence: self.queue_sequence,
            queue_operation: self.queue_operation,
            bash_result: self.bash_result.clone(),
            bash_result_row_id: self.bash_result_row_id,
            bash_notice_identity: self.bash_notice_identity.clone(),
            bash_should_query: self.bash_should_query,
            original_client_turn_id: self.original_client_turn_id,
            queue_delivery: self.queue_delivery,
            coalesced_batch: self.coalesced_batch.clone(),
            revision: self.revision,
            state: self.state.clone(),
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            safe_to_resume: self.safe_to_resume,
            reason: self.reason.clone(),
            events: Vec::new(),
        }
    }

    fn from_parts(metadata: StoredTurnMetadata, journal_events: Vec<DurableTurnEvent>) -> Self {
        // Older metadata files embedded their retained events. If a process
        // appends to one of those files, the new journal contains only the
        // post-migration suffix, so recovery must merge both sources instead
        // of silently discarding the legacy prefix.
        let metadata_last_sequence = metadata.last_sequence;
        let mut events = metadata.events;
        events.extend(journal_events);
        events.sort_unstable_by_key(|event| event.sequence);
        events.dedup_by_key(|event| event.sequence);

        let mut checkpoint = Self {
            session_id: metadata.session_id,
            turn_id: metadata.turn_id,
            prompt: metadata.prompt,
            prompt_identity: metadata.prompt_identity,
            skill_prompt: metadata.skill_prompt,
            prompt_mode: metadata.prompt_mode,
            images: metadata.images,
            queue_sequence: metadata.queue_sequence,
            queue_operation: metadata.queue_operation,
            bash_result: metadata.bash_result,
            bash_result_row_id: metadata.bash_result_row_id,
            bash_notice_identity: metadata.bash_notice_identity,
            bash_should_query: metadata.bash_should_query,
            original_client_turn_id: metadata.original_client_turn_id,
            queue_delivery: metadata.queue_delivery,
            coalesced_batch: metadata.coalesced_batch,
            revision: metadata.revision,
            state: metadata.state,
            first_sequence: metadata.first_sequence,
            last_sequence: metadata.last_sequence,
            safe_to_resume: metadata.safe_to_resume,
            reason: metadata.reason,
            events,
        };
        checkpoint.reconcile_sequences();

        // The journal is the commit record for streamed events. A process can
        // die after appending a terminal event but before the follow-up
        // metadata transition, leaving a stale `Running` metadata file. On a
        // cold load, the last retained terminal event is therefore
        // authoritative. An explicit cancel remains stronger than any late
        // terminal event emitted while the cancelled executor unwinds.
        if checkpoint.state.is_some() && checkpoint.state != Some(TurnRecoveryStateDto::Cancelled) {
            if let Some((state, reason)) = checkpoint
                .events
                .iter()
                .rev()
                .find_map(|event| terminal_state_from_event(&event.event_json))
            {
                checkpoint.state = Some(state);
                checkpoint.safe_to_resume = false;
                checkpoint.reason = reason;
                checkpoint.queue_delivery =
                    if checkpoint.state == Some(TurnRecoveryStateDto::Cancelled) {
                        QueueDeliveryState::Cancelled
                    } else {
                        QueueDeliveryState::Delivered
                    };
            }
        }

        // Appends no longer rewrite metadata. Reconstruct the revision for a
        // fresh process from the journal suffix that was not reflected in the
        // metadata snapshot. This keeps sequence/revision monotonic without
        // paying an atomic metadata write for every token.
        checkpoint.revision = checkpoint.revision.saturating_add(
            checkpoint
                .last_sequence
                .saturating_sub(metadata_last_sequence),
        );
        checkpoint
    }

    fn reconcile_sequences(&mut self) {
        retain_recent_events(&mut self.events);
        if let Some(first) = self.events.first() {
            self.first_sequence = first.sequence;
            self.last_sequence = self.last_sequence.max(
                self.events
                    .last()
                    .map_or(first.sequence, |event| event.sequence),
            );
        } else if self.last_sequence == 0 {
            self.first_sequence = 0;
        } else {
            self.first_sequence = self.last_sequence.saturating_add(1);
        }

        if self
            .state
            .as_ref()
            .is_some_and(TurnRecoveryStateDto::is_terminal)
        {
            self.safe_to_resume = false;
        }
    }
}

#[derive(Deserialize)]
struct RetainedEventEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    outcome: Option<RetainedOutcomeEnvelope>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct RetainedOutcomeEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

fn is_resume_safe_retained_event(event_json: &str) -> bool {
    serde_json::from_str::<RetainedEventEnvelope>(event_json)
        .map(|event| matches!(event.kind.as_str(), "turn_started"))
        .unwrap_or(false)
}

fn terminal_state_from_event(event_json: &str) -> Option<(TurnRecoveryStateDto, Option<String>)> {
    let event = serde_json::from_str::<RetainedEventEnvelope>(event_json).ok()?;
    match event.kind.as_str() {
        "turn_ended" => match event.outcome?.kind.as_str() {
            "end_turn" => Some((TurnRecoveryStateDto::Completed, None)),
            "max_turns" => Some((
                TurnRecoveryStateDto::Failed,
                Some(event.stop_reason.unwrap_or_else(|| "max_turns".to_string())),
            )),
            "cancelled" => Some((
                TurnRecoveryStateDto::Cancelled,
                Some("cancelled".to_string()),
            )),
            _ => None,
        },
        "error" => Some((TurnRecoveryStateDto::Failed, event.message)),
        _ => None,
    }
}

/// Normal token deltas only need an ordered write/close. Force the journal
/// through the storage barrier at boundaries whose loss could cause a replay
/// or side-effect decision to diverge after process death.
fn event_requires_durable_flush(event_json: &str) -> bool {
    serde_json::from_str::<RetainedEventEnvelope>(event_json)
        .map(|event| {
            matches!(
                event.kind.as_str(),
                "turn_started"
                    | "tool_use_started"
                    | "tool_use_result"
                    | "ask_user_question"
                    | "ask_user_question_resolved"
                    | "permission_request_resolved"
                    | "turn_ended"
                    | "error"
            )
        })
        .unwrap_or(false)
}

fn retain_recent_events(events: &mut Vec<DurableTurnEvent>) {
    if events.len() > MAX_RETAINED_EVENTS {
        let excess = events.len() - MAX_RETAINED_EVENTS;
        events.drain(..excess);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeDisposition {
    Queued,
    Ready,
    WaitingForUser,
    Terminal,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DurableTurnStoreError {
    #[error("invalid durable turn identity: {0}")]
    InvalidIdentity(String),
    #[error("durable turn {turn_id} does not exist in session {session_id}")]
    NotFound { session_id: String, turn_id: u64 },
    #[error("durable turn {turn_id} already exists in session {session_id}")]
    AlreadyExists { session_id: String, turn_id: u64 },
    #[error("durable turn {turn_id} in session {session_id} is already terminal")]
    Terminal { session_id: String, turn_id: u64 },
    #[error("durable turn {turn_id} in session {session_id} has not been claimed")]
    NotClaimed { session_id: String, turn_id: u64 },
    #[error("durable turn storage failure: {0}")]
    Storage(String),
}

/// One process owns this store at a time. Metadata stays atomic while retained
/// events live in a crash-tolerant append journal with bounded compaction.
pub(crate) struct DurableTurnStore {
    root: PathBuf,
    lock: Mutex<HashMap<PathBuf, CachedTurn>>,
    /// The first scan indexes checkpoint filenames for a session. Store writes
    /// keep an existing index current so later admissions do not rescan disk.
    session_turn_ids: Mutex<HashMap<String, Vec<u64>>>,
    #[cfg(test)]
    journal_sync_count: AtomicUsize,
}

impl DurableTurnStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(HashMap::new()),
            session_turn_ids: Mutex::new(HashMap::new()),
            #[cfg(test)]
            journal_sync_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn begin(
        &self,
        session_id: &str,
        turn_id: u64,
        prompt: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.begin_with_skill_prompt(session_id, turn_id, prompt, prompt_mode, images, None)
    }

    pub(crate) fn begin_with_skill_prompt(
        &self,
        session_id: &str,
        turn_id: u64,
        prompt: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
        skill_prompt: Option<DurableSkillPrompt>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.begin_direct_with_skill_prompt(
            session_id,
            turn_id,
            Some(turn_id),
            prompt,
            prompt_mode,
            images,
            skill_prompt,
        )
    }

    pub(crate) fn begin_direct_with_skill_prompt(
        &self,
        session_id: &str,
        turn_id: u64,
        original_client_turn_id: Option<u64>,
        prompt: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
        skill_prompt: Option<DurableSkillPrompt>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let relative = relative_path(session_id, turn_id)?;
        let exists = self
            .root
            .join(&relative)
            .try_exists()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if exists {
            let existing = self.read_relative(&relative)?;
            return Err(DurableTurnStoreError::AlreadyExists {
                session_id: existing.checkpoint.session_id,
                turn_id: existing.checkpoint.turn_id,
            });
        }

        let checkpoint = DurableTurnCheckpoint {
            session_id: session_id.to_string(),
            turn_id,
            prompt,
            prompt_identity: skill_prompt.is_none().then(DurablePromptIdentity::new),
            skill_prompt,
            prompt_mode,
            images,
            queue_sequence: 0,
            queue_operation: QueueOperationKind::Prompt,
            bash_result: None,
            bash_result_row_id: None,
            bash_notice_identity: (prompt_mode == Some(PromptModeDto::Bash))
                .then(DurablePromptIdentity::new),
            bash_should_query: None,
            original_client_turn_id,
            queue_delivery: QueueDeliveryState::Claimed,
            coalesced_batch: None,
            revision: 1,
            state: Some(TurnRecoveryStateDto::Running),
            first_sequence: 0,
            last_sequence: 0,
            safe_to_resume: true,
            reason: None,
            events: Vec::new(),
        };
        self.write_metadata_relative(&relative, &checkpoint)?;
        cache.insert(
            relative,
            CachedTurn {
                checkpoint: checkpoint.clone(),
                journal_event_count: 0,
            },
        );
        self.remember_turn_id(session_id, turn_id);
        Ok(checkpoint)
    }

    /// Atomically accept a busy SendPrompt before it is put into the volatile
    /// dispatch cache. Duplicate client IDs are idempotent only when the full
    /// source payload matches.
    pub(crate) fn admit_queued_prompt(
        &self,
        session_id: &str,
        turn_id: Option<u64>,
        original_client_turn_id: Option<u64>,
        prompt: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.admit_queued_operation(
            QueueOperationKind::Prompt,
            session_id,
            turn_id,
            original_client_turn_id,
            prompt,
            prompt_mode,
            images,
        )
    }

    pub(crate) fn admit_queued_slash(
        &self,
        session_id: &str,
        turn_id: Option<u64>,
        original_client_turn_id: Option<u64>,
        raw: Utf16JsonProjection,
        images: Vec<ImageRefDto>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.admit_queued_operation(
            QueueOperationKind::Slash,
            session_id,
            turn_id,
            original_client_turn_id,
            raw,
            None,
            images,
        )
    }

    fn admit_queued_operation(
        &self,
        queue_operation: QueueOperationKind,
        session_id: &str,
        turn_id: Option<u64>,
        original_client_turn_id: Option<u64>,
        prompt: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        let turn_id = match turn_id {
            Some(turn_id) => turn_id,
            None => checkpoints
                .iter()
                .map(|checkpoint| checkpoint.turn_id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| {
                    DurableTurnStoreError::Storage(
                        "mobile turn id space is exhausted for this session".into(),
                    )
                })?,
        };
        let relative = relative_path(session_id, turn_id)?;
        if self
            .root
            .join(&relative)
            .try_exists()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?
        {
            let existing = self.read_relative(&relative)?.checkpoint;
            if existing.prompt == prompt
                && existing.queue_operation == queue_operation
                && existing.prompt_mode == prompt_mode
                && existing.images == images
                && existing.original_client_turn_id == original_client_turn_id
                && matches!(
                    existing.queue_delivery,
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                )
            {
                return Ok(existing);
            }
            return Err(DurableTurnStoreError::AlreadyExists {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        let queue_sequence = checkpoints
            .iter()
            .map(|checkpoint| checkpoint.queue_sequence)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "mobile prompt queue sequence space is exhausted for this session".into(),
                )
            })?;
        let checkpoint = DurableTurnCheckpoint {
            session_id: session_id.to_string(),
            turn_id,
            prompt,
            prompt_identity: Some(DurablePromptIdentity::new()),
            skill_prompt: None,
            prompt_mode,
            images,
            queue_sequence,
            queue_operation,
            bash_result: None,
            bash_result_row_id: None,
            bash_notice_identity: (prompt_mode == Some(PromptModeDto::Bash))
                .then(DurablePromptIdentity::new),
            bash_should_query: None,
            original_client_turn_id,
            queue_delivery: QueueDeliveryState::Pending,
            coalesced_batch: None,
            revision: 1,
            state: None,
            first_sequence: 0,
            last_sequence: 0,
            safe_to_resume: false,
            reason: None,
            events: Vec::new(),
        };
        self.write_metadata_relative(&relative, &checkpoint)?;
        cache.insert(
            relative,
            CachedTurn {
                checkpoint: checkpoint.clone(),
                journal_event_count: 0,
            },
        );
        self.remember_turn_id(session_id, turn_id);
        Ok(checkpoint)
    }

    /// Rebuild the volatile Mobile dispatch cache from the active session's
    /// private queued checkpoints. Returned entries are in admission order.
    pub(crate) fn scan_pending(
        &self,
        session_id: &str,
    ) -> Result<Vec<DurableTurnCheckpoint>, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        let coalesced_members = checkpoints
            .iter()
            .filter_map(|checkpoint| checkpoint.coalesced_batch.as_ref())
            .flat_map(|batch| batch.member_turn_ids.iter().copied())
            .collect::<std::collections::HashSet<_>>();
        checkpoints.retain(|checkpoint| {
            checkpoint.state.is_none()
                && (!coalesced_members.contains(&checkpoint.turn_id)
                    || checkpoint.coalesced_batch.is_some())
                && matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending
                        | QueueDeliveryState::CoalescedPending
                        | QueueDeliveryState::CoalescedClaimed
                )
        });
        checkpoints.sort_by_key(|checkpoint| (checkpoint.queue_sequence, checkpoint.turn_id));
        Ok(checkpoints)
    }

    /// Persist the aggregate row identity and claim the in-turn delivery under
    /// one store lock. The first member owns the envelope; Native's coalesced
    /// row identity comes from the last member.
    pub(crate) fn prepare_mid_turn_batch(
        &self,
        session_id: &str,
        member_turn_ids: &[u64],
    ) -> Result<CoalescedPromptBatch, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        let members = member_turn_ids
            .iter()
            .map(|turn_id| {
                checkpoints
                    .iter()
                    .find(|checkpoint| checkpoint.turn_id == *turn_id)
                    .cloned()
                    .ok_or_else(|| DurableTurnStoreError::NotFound {
                        session_id: session_id.to_string(),
                        turn_id: *turn_id,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if members
            .iter()
            .any(|member| member.queue_operation != QueueOperationKind::Prompt)
        {
            return Err(DurableTurnStoreError::Storage(
                "mobile prompt batch contains a non-prompt operation".into(),
            ));
        }
        let first = members.first().ok_or_else(|| {
            DurableTurnStoreError::Storage("cannot persist an empty mobile prompt batch".into())
        })?;
        let first_is_bash = first.prompt_mode == Some(PromptModeDto::Bash);
        if members
            .iter()
            .any(|member| (member.prompt_mode == Some(PromptModeDto::Bash)) != first_is_bash)
            || (first_is_bash && members.len() != 1)
        {
            return Err(DurableTurnStoreError::Storage(
                "mobile prompt batch crosses a Bash execution boundary".into(),
            ));
        }
        if members
            .windows(2)
            .any(|pair| pair[0].queue_sequence >= pair[1].queue_sequence)
        {
            return Err(DurableTurnStoreError::Storage(
                "mobile prompt batch does not follow durable admission order".into(),
            ));
        }
        if member_turn_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != member_turn_ids.len()
        {
            return Err(DurableTurnStoreError::Storage(
                "mobile prompt batch repeats a durable member".into(),
            ));
        }
        let owner = first.turn_id;
        if members.iter().enumerate().any(|(index, member)| {
            let owner_retry = index == 0
                && member.queue_delivery == QueueDeliveryState::CoalescedClaimed
                && member
                    .coalesced_batch
                    .as_ref()
                    .is_some_and(|batch| batch.member_turn_ids == member_turn_ids);
            member.state.is_some()
                || (!owner_retry
                    && !matches!(
                        member.queue_delivery,
                        QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                    ))
        }) {
            return Err(DurableTurnStoreError::Terminal {
                session_id: session_id.to_string(),
                turn_id: owner,
            });
        }
        let aggregate_identity = members
            .last()
            .and_then(|member| member.prompt_identity.clone())
            .ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "last mobile prompt batch member is missing its durable row identity".into(),
                )
            })?;
        let member_identities = members
            .iter()
            .map(|member| {
                Ok(CoalescedPromptMemberIdentity {
                    turn_id: member.turn_id,
                    original_client_turn_id: member.original_client_turn_id,
                    prompt_identity: member.prompt_identity.clone().ok_or_else(|| {
                        DurableTurnStoreError::Storage(format!(
                            "mobile prompt batch member {} is missing its durable row identity",
                            member.turn_id
                        ))
                    })?,
                })
            })
            .collect::<Result<Vec<_>, DurableTurnStoreError>>()?;
        let batch = CoalescedPromptBatch {
            owner_turn_id: owner,
            member_turn_ids: member_turn_ids.to_vec(),
            member_identities,
            aggregate_identity,
        };
        self.update_if_locked(&mut cache, session_id, owner, |checkpoint| {
            if checkpoint.state.is_some()
                || !matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending
                        | QueueDeliveryState::CoalescedPending
                        | QueueDeliveryState::CoalescedClaimed
                )
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id: owner,
                });
            }
            if checkpoint
                .coalesced_batch
                .as_ref()
                .is_some_and(|existing| existing != &batch)
            {
                return Err(DurableTurnStoreError::Storage(
                    "mobile prompt batch identity changed after it was persisted".into(),
                ));
            }
            checkpoint.coalesced_batch = Some(batch.clone());
            checkpoint.queue_delivery = QueueDeliveryState::CoalescedClaimed;
            Ok(Some(()))
        })?;
        Ok(batch)
    }

    /// Acknowledge the coalesced input only after its append-once history row
    /// was accepted. The owner envelope remains the cold-recovery commit marker.
    pub(crate) fn acknowledge_mid_turn_batch(
        &self,
        session_id: &str,
        owner_turn_id: u64,
        member_turn_ids: &[u64],
    ) -> Result<(), DurableTurnStoreError> {
        if member_turn_ids.first().copied() != Some(owner_turn_id) {
            return Err(DurableTurnStoreError::Storage(
                "mobile prompt acknowledgement does not start with its owner".into(),
            ));
        }
        self.update(session_id, owner_turn_id, |checkpoint| {
            let Some(batch) = checkpoint.coalesced_batch.as_ref() else {
                return Err(DurableTurnStoreError::NotClaimed {
                    session_id: session_id.to_string(),
                    turn_id: owner_turn_id,
                });
            };
            if batch.owner_turn_id != owner_turn_id || batch.member_turn_ids != member_turn_ids {
                return Err(DurableTurnStoreError::Storage(
                    "mobile prompt acknowledgement does not match its persisted batch".into(),
                ));
            }
            if checkpoint.queue_delivery == QueueDeliveryState::Delivered {
                return Ok(());
            }
            if checkpoint.state.is_some()
                || checkpoint.queue_delivery != QueueDeliveryState::CoalescedClaimed
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id: owner_turn_id,
                });
            }
            // The owner metadata is the batch commit marker. Its one atomic
            // write retires every member, even if the process stops before the
            // volatile queue commands are removed.
            checkpoint.queue_delivery = QueueDeliveryState::Delivered;
            Ok(())
        })
    }

    pub(crate) fn claim_pending(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        let claimed = self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_some()
                || !matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending
                        | QueueDeliveryState::CoalescedPending
                        | QueueDeliveryState::CoalescedClaimed
                )
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = Some(TurnRecoveryStateDto::Running);
            checkpoint.queue_delivery = QueueDeliveryState::Claimed;
            checkpoint.safe_to_resume = true;
            checkpoint.reason = None;
            Ok(checkpoint.clone())
        })?;
        Ok(claimed)
    }

    /// Atomically cross the exactly-once dispatch boundary for a raw slash
    /// operation. Claimed entries are not rebuilt after restart because the
    /// side effect may have happened before the process stopped.
    pub(crate) fn claim_queued_slash(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.queue_operation != QueueOperationKind::Slash
                || checkpoint.state.is_some()
                || checkpoint.queue_delivery != QueueDeliveryState::Pending
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.queue_delivery = QueueDeliveryState::SlashClaimed;
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("slash_dispatch_claimed".into());
            Ok(checkpoint.clone())
        })
    }

    /// Enter the non-replayable Shell side-effect boundary. Publish Running
    /// while execution is owned, but keep recovery unsafe until the exact
    /// result has been written into this checkpoint.
    pub(crate) fn claim_queued_bash(
        &self,
        session_id: &str,
        turn_id: u64,
        should_query: bool,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.queue_operation != QueueOperationKind::Prompt
                || checkpoint.prompt_mode != Some(PromptModeDto::Bash)
                || checkpoint.state.is_some()
                || checkpoint.queue_delivery != QueueDeliveryState::Pending
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = Some(TurnRecoveryStateDto::Running);
            checkpoint.bash_should_query = Some(should_query);
            checkpoint.queue_delivery = QueueDeliveryState::BashClaimed;
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("bash_execution_claimed".into());
            Ok(checkpoint.clone())
        })
    }

    /// Persist the completed external result before making the model follow-up
    /// recoverable. A restart from this point replays the rows and request but
    /// never runs the Shell command again.
    pub(crate) fn prepare_claimed_bash_turn(
        &self,
        session_id: &str,
        turn_id: u64,
        result: DurableBashResult,
        still_should_query: bool,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.queue_operation != QueueOperationKind::Prompt
                || checkpoint.prompt_mode != Some(PromptModeDto::Bash)
                || checkpoint.state != Some(TurnRecoveryStateDto::Running)
                || checkpoint.queue_delivery != QueueDeliveryState::BashClaimed
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            let command_units = checkpoint.prompt.string_units("").ok_or_else(|| {
                DurableTurnStoreError::Storage("Bash checkpoint command is not text".into())
            })?;
            let mut input_units = "<bash-input>".encode_utf16().collect::<Vec<_>>();
            input_units.extend(command_units);
            input_units.extend("</bash-input>".encode_utf16());
            checkpoint.prompt = Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&input_units),
                input_units,
            )
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
            let configured_to_query = checkpoint.bash_should_query.ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "claimed Bash prompt is missing its response setting".into(),
                )
            })?;
            checkpoint.bash_should_query =
                Some(configured_to_query && still_should_query && !result.interrupted);
            checkpoint.bash_result = Some(result);
            checkpoint.bash_result_row_id = Some(MessageId::new());
            checkpoint.queue_delivery = QueueDeliveryState::Claimed;
            checkpoint.safe_to_resume = true;
            checkpoint.reason = None;
            Ok(checkpoint.clone())
        })
    }

    /// Rebind a claimed slash's durable identity to its model follow-up.
    /// Dispatch remains at-most-once, while the resulting prompt can use the
    /// ordinary turn recovery and append-once path.
    pub(crate) fn prepare_claimed_slash_turn(
        &self,
        session_id: &str,
        turn_id: u64,
        prompt: Utf16JsonProjection,
        skill_prompt: Option<DurableSkillPrompt>,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.queue_operation != QueueOperationKind::Slash
                || checkpoint.state.is_some()
                || checkpoint.queue_delivery != QueueDeliveryState::SlashClaimed
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.prompt = prompt;
            checkpoint.prompt_identity = skill_prompt.is_none().then(|| {
                checkpoint
                    .prompt_identity
                    .clone()
                    .unwrap_or_else(DurablePromptIdentity::new)
            });
            checkpoint.skill_prompt = skill_prompt;
            checkpoint.prompt_mode = None;
            checkpoint.state = Some(TurnRecoveryStateDto::Running);
            checkpoint.queue_delivery = QueueDeliveryState::Claimed;
            checkpoint.safe_to_resume = true;
            checkpoint.reason = None;
            Ok(checkpoint.clone())
        })
    }

    pub(crate) fn complete_claimed_slash(
        &self,
        session_id: &str,
        turn_id: u64,
        reason: Option<String>,
    ) -> Result<(), DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.queue_operation != QueueOperationKind::Slash
                || checkpoint.queue_delivery != QueueDeliveryState::SlashClaimed
                || checkpoint.state.is_some()
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.queue_delivery = QueueDeliveryState::Delivered;
            checkpoint.safe_to_resume = false;
            checkpoint.reason = reason;
            Ok(())
        })
    }

    fn read_session_checkpoints(
        &self,
        cache: &mut HashMap<PathBuf, CachedTurn>,
        session_id: &str,
    ) -> Result<Vec<DurableTurnCheckpoint>, DurableTurnStoreError> {
        relative_path(session_id, 0)?;
        let indexed_ids = self
            .session_turn_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned();
        let turn_ids = if let Some(indexed_ids) = indexed_ids {
            indexed_ids
        } else {
            let directory = self.root.join(session_id);
            let mut turn_ids = Vec::new();
            if directory
                .try_exists()
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?
            {
                let entries = std::fs::read_dir(&directory)
                    .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
                for entry in entries {
                    let entry =
                        entry.map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
                    if !entry
                        .file_type()
                        .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?
                        .is_file()
                    {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    let Some(id) = name.strip_suffix(".json") else {
                        continue;
                    };
                    let Ok(turn_id) = id.parse::<u64>() else {
                        continue;
                    };
                    turn_ids.push(turn_id);
                }
            }
            turn_ids.sort_unstable();
            turn_ids.dedup();
            self.session_turn_ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session_id.to_string(), turn_ids.clone());
            turn_ids
        };
        let mut checkpoints = Vec::with_capacity(turn_ids.len());
        for turn_id in turn_ids {
            let relative = relative_path(session_id, turn_id)?;
            let cached = self.load_cached_turn(cache, &relative)?;
            if cached.checkpoint.session_id != session_id || cached.checkpoint.turn_id != turn_id {
                return Err(DurableTurnStoreError::InvalidIdentity(format!(
                    "checkpoint path {session_id}/{turn_id}.json does not match its metadata"
                )));
            }
            checkpoints.push(cached.checkpoint.clone());
        }
        Ok(checkpoints)
    }

    fn remember_turn_id(&self, session_id: &str, turn_id: u64) {
        let mut index = self
            .session_turn_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(turn_ids) = index.get_mut(session_id) else {
            return;
        };
        match turn_ids.binary_search(&turn_id) {
            Ok(_) => {}
            Err(position) => turn_ids.insert(position, turn_id),
        }
    }

    pub(crate) fn load(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<DurableTurnCheckpoint, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let relative = relative_path(session_id, turn_id)?;
        let exists = self
            .root
            .join(&relative)
            .try_exists()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if !exists {
            return Err(DurableTurnStoreError::NotFound {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        self.load_cached_turn(&mut cache, &relative)
            .map(|cached| cached.checkpoint.clone())
    }

    pub(crate) fn append_event(
        &self,
        session_id: &str,
        turn_id: u64,
        event_json: String,
    ) -> Result<DurableTurnEvent, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let relative = relative_path(session_id, turn_id)?;
        let (event, persist_result) = {
            // Keep the in-memory checkpoint owned by the cache. Cloning the
            // retained event vector for every token makes an append stream
            // quadratic even though the on-disk operation is journal append.
            // The cache is invalidated below if either durable write fails.
            let cached = self.load_cached_turn(&mut cache, &relative)?;
            let terminal_event = terminal_state_from_event(&event_json).is_some();
            let Some(state) = cached.checkpoint.state.as_ref() else {
                return Err(DurableTurnStoreError::NotClaimed {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            };
            if state.is_terminal() && !(state == &TurnRecoveryStateDto::Cancelled && terminal_event)
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }

            let sequence = cached.checkpoint.last_sequence.saturating_add(1);
            let event = DurableTurnEvent {
                sequence,
                event_json,
            };

            let mut journal_event_count = cached.journal_event_count.saturating_add(1);
            let persist_result = if journal_event_count > max_journal_events_before_compaction() {
                // Retain a bounded suffix only at compaction boundaries. The
                // slack window avoids an O(retained_events) drain on every
                // append once the retention limit is reached.
                let mut checkpoint = cached.checkpoint.clone();
                checkpoint.last_sequence = sequence;
                if checkpoint.first_sequence == 0 {
                    checkpoint.first_sequence = sequence;
                }
                checkpoint.events.push(event.clone());
                checkpoint.revision = checkpoint.revision.saturating_add(1);
                checkpoint.reconcile_sequences();
                let result = self
                    .write_compacted_journal_relative(&relative, &checkpoint.events)
                    // Compaction is a semantic persistence boundary: publish
                    // the new cursor only after the compacted journal has been
                    // atomically replaced.
                    .and_then(|()| self.write_metadata_relative(&relative, &checkpoint));
                if result.is_ok() {
                    journal_event_count = checkpoint.events.len();
                    cached.checkpoint = checkpoint;
                    cached.journal_event_count = journal_event_count;
                }
                result
            } else {
                // Keep normal deltas off the metadata/fsync hot path. The
                // append write is acknowledged only after write/close; tool,
                // permission, turn-start, and terminal boundaries additionally
                // force the journal before the event is delivered.
                let result = self.append_journal_relative(
                    &relative,
                    &event,
                    event_requires_durable_flush(&event.event_json),
                );
                if result.is_ok() {
                    cached.checkpoint.last_sequence = sequence;
                    if cached.checkpoint.first_sequence == 0 {
                        cached.checkpoint.first_sequence = sequence;
                    }
                    cached.checkpoint.events.push(event.clone());
                    cached.checkpoint.revision = cached.checkpoint.revision.saturating_add(1);
                    cached.journal_event_count = journal_event_count;
                }
                result
            };

            (event, persist_result)
        };

        match persist_result {
            Ok(()) => Ok(event),
            Err(error) => {
                // The cache may contain an event that was only partially
                // persisted (or metadata that did not make it to its atomic
                // rename). Force the next operation through recovery.
                cache.remove(&relative);
                Err(error)
            }
        }
    }

    pub(crate) fn transition(
        &self,
        session_id: &str,
        turn_id: u64,
        state: TurnRecoveryStateDto,
        safe_to_resume: bool,
        reason: Option<String>,
    ) -> Result<TurnRecoverySnapshotDto, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            let Some(current_state) = checkpoint.state.as_ref() else {
                return Err(DurableTurnStoreError::NotClaimed {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            };
            if current_state.is_terminal() && current_state != &state {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = Some(state.clone());
            checkpoint.safe_to_resume = safe_to_resume && !state.is_terminal();
            checkpoint.reason = reason;
            if state.is_terminal() && state != TurnRecoveryStateDto::Cancelled {
                checkpoint.queue_delivery = QueueDeliveryState::Delivered;
            }
            Ok(checkpoint.snapshot())
        })
    }

    /// Live observers cannot undo a pause or terminal checkpoint. Compare and
    /// mutate under the same store lock as explicit pause/resume transitions.
    pub(crate) fn transition_if_live(
        &self,
        session_id: &str,
        turn_id: u64,
        state: TurnRecoveryStateDto,
        safe_to_resume: bool,
        reason: Option<String>,
    ) -> Result<Option<TurnRecoverySnapshotDto>, DurableTurnStoreError> {
        self.update_if(session_id, turn_id, |checkpoint| {
            if !matches!(
                checkpoint.state.as_ref(),
                Some(TurnRecoveryStateDto::Running | TurnRecoveryStateDto::WaitingForUser)
            ) {
                return Ok(None);
            }
            checkpoint.state = Some(state.clone());
            checkpoint.safe_to_resume = safe_to_resume && !state.is_terminal();
            checkpoint.reason = reason;
            if state.is_terminal() && state != TurnRecoveryStateDto::Cancelled {
                checkpoint.queue_delivery = QueueDeliveryState::Delivered;
            }
            Ok(Some(checkpoint.snapshot()))
        })
    }

    pub(crate) fn cancel(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<Option<TurnRecoverySnapshotDto>, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        if checkpoints.iter().any(|checkpoint| {
            checkpoint.coalesced_batch.as_ref().is_some_and(|batch| {
                batch.member_turn_ids.contains(&turn_id)
                    && batch.owner_turn_id == checkpoint.turn_id
                    && checkpoint.state.is_none()
                    && checkpoint.queue_delivery == QueueDeliveryState::CoalescedClaimed
            })
        }) {
            return Err(DurableTurnStoreError::Terminal {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        let cancelled = self.update_if_locked(&mut cache, session_id, turn_id, |checkpoint| {
            if checkpoint
                .state
                .as_ref()
                .is_some_and(TurnRecoveryStateDto::is_terminal)
                || matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Delivered | QueueDeliveryState::Cancelled
                )
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            let is_private_queue_entry = checkpoint.state.is_none()
                && matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending
                        | QueueDeliveryState::CoalescedPending
                        | QueueDeliveryState::SlashClaimed
                );
            if !is_private_queue_entry {
                checkpoint.state = Some(TurnRecoveryStateDto::Cancelled);
            }
            checkpoint.queue_delivery = QueueDeliveryState::Cancelled;
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("explicit_cancel".to_string());
            Ok(Some(
                (!is_private_queue_entry).then(|| checkpoint.snapshot()),
            ))
        })?;
        Ok(cancelled.flatten())
    }

    /// Keep a live Bash execution claim while its owning process is being
    /// killed and joined. `cancel()` remains terminal for cold/inactive
    /// checkpoints, where no executor can still produce the exact interrupted
    /// result rows.
    pub(crate) fn mark_claimed_bash_cancel_pending(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<TurnRecoverySnapshotDto, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            let is_unresolved_claim = checkpoint.state == Some(TurnRecoveryStateDto::Running)
                && checkpoint.queue_delivery == QueueDeliveryState::BashClaimed;
            let has_durable_result = checkpoint.state == Some(TurnRecoveryStateDto::Running)
                && checkpoint.queue_delivery == QueueDeliveryState::Claimed
                && checkpoint.bash_result.is_some();
            if checkpoint.queue_operation != QueueOperationKind::Prompt
                || checkpoint.prompt_mode != Some(PromptModeDto::Bash)
                || (!is_unresolved_claim && !has_durable_result)
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.bash_should_query = Some(false);
            if is_unresolved_claim {
                checkpoint.safe_to_resume = false;
            }
            checkpoint.reason = Some("explicit_cancel".to_string());
            Ok(checkpoint.snapshot())
        })
    }

    /// Cancel only an unclaimed private queue entry. Stale cancellation for an
    /// unrelated live or paused turn must not disturb that turn's owner.
    pub(crate) fn cancel_pending(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<Option<Vec<u64>>, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        let Some(target) = checkpoints
            .iter()
            .find(|checkpoint| checkpoint.turn_id == turn_id)
        else {
            return Err(DurableTurnStoreError::NotFound {
                session_id: session_id.to_string(),
                turn_id,
            });
        };
        let owner = checkpoints.iter().find(|checkpoint| {
            checkpoint.coalesced_batch.as_ref().is_some_and(|batch| {
                batch.member_turn_ids.contains(&turn_id)
                    && batch.owner_turn_id == checkpoint.turn_id
                    && checkpoint.state.is_none()
            })
        });
        if owner.is_some_and(|checkpoint| {
            checkpoint.queue_delivery == QueueDeliveryState::CoalescedClaimed
        }) {
            // The owner envelope is the irreversible admission point. The
            // host routes cancellation through the already-owned active turn.
            return Ok(None);
        }
        if target.state.is_some()
            || !matches!(
                target.queue_delivery,
                QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
            )
        {
            return Ok(None);
        }
        let member_ids = owner
            .and_then(|checkpoint| checkpoint.coalesced_batch.as_ref())
            .map(|batch| batch.member_turn_ids.clone())
            .unwrap_or_else(|| vec![turn_id]);
        let owner_id = owner.map(|checkpoint| checkpoint.turn_id);
        // Mark the batch owner first. Its envelope is the cold-recovery commit
        // marker, so a write failure while cancelling later members cannot
        // resurrect any prompt from the group.
        let mut update_ids = member_ids.clone();
        if let Some(owner_id) = owner_id {
            update_ids.retain(|member_id| *member_id != owner_id);
            update_ids.insert(0, owner_id);
        }
        for member_id in &update_ids {
            self.update_if_locked(&mut cache, session_id, *member_id, |checkpoint| {
                if checkpoint.state.is_some()
                    || !matches!(
                        checkpoint.queue_delivery,
                        QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                    )
                {
                    return Ok(None);
                }
                checkpoint.queue_delivery = QueueDeliveryState::Cancelled;
                checkpoint.safe_to_resume = false;
                checkpoint.reason = Some("explicit_cancel".to_string());
                Ok(Some(()))
            })?;
        }
        Ok(Some(member_ids))
    }

    pub(crate) fn coalesced_owner_for_member(
        &self,
        session_id: &str,
        member_turn_id: u64,
    ) -> Result<Option<u64>, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        Ok(checkpoints.iter().find_map(|checkpoint| {
            checkpoint
                .coalesced_batch
                .as_ref()
                .filter(|batch| batch.member_turn_ids.contains(&member_turn_id))
                .map(|_| checkpoint.turn_id)
        }))
    }

    pub(crate) fn resume(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(ResumeDisposition, Option<TurnRecoverySnapshotDto>), DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_none() {
                return match checkpoint.queue_delivery {
                    QueueDeliveryState::Pending
                    | QueueDeliveryState::CoalescedPending
                    | QueueDeliveryState::CoalescedClaimed => Ok((ResumeDisposition::Queued, None)),
                    QueueDeliveryState::SlashClaimed => Ok((ResumeDisposition::Terminal, None)),
                    QueueDeliveryState::BashClaimed => {
                        Ok((ResumeDisposition::WaitingForUser, None))
                    }
                    QueueDeliveryState::Delivered | QueueDeliveryState::Cancelled => {
                        Ok((ResumeDisposition::Terminal, None))
                    }
                    QueueDeliveryState::Claimed => Err(DurableTurnStoreError::NotClaimed {
                        session_id: session_id.to_string(),
                        turn_id,
                    }),
                };
            }
            let state =
                checkpoint
                    .state
                    .as_ref()
                    .ok_or_else(|| DurableTurnStoreError::NotClaimed {
                        session_id: session_id.to_string(),
                        turn_id,
                    })?;
            let disposition = if state.is_terminal() {
                ResumeDisposition::Terminal
            } else if checkpoint.can_resume_without_user() {
                checkpoint.state = Some(TurnRecoveryStateDto::Running);
                checkpoint.reason = None;
                ResumeDisposition::Ready
            } else {
                checkpoint.state = Some(TurnRecoveryStateDto::WaitingForUser);
                checkpoint.safe_to_resume = false;
                if checkpoint.reason.is_none() {
                    checkpoint.reason = Some("replay_boundary_requires_user".to_string());
                }
                ResumeDisposition::WaitingForUser
            };
            Ok((disposition, Some(checkpoint.snapshot())))
        })
    }

    #[cfg(test)]
    pub(crate) fn retain_events_from_for_test(
        &self,
        session_id: &str,
        turn_id: u64,
        first_sequence: u64,
    ) -> Result<(), DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            checkpoint
                .events
                .retain(|event| event.sequence >= first_sequence);
            checkpoint.reconcile_sequences();
            Ok(())
        })?;

        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let relative = relative_path(session_id, turn_id)?;
        let cached = self.load_cached_turn(&mut cache, &relative)?.clone();
        self.write_compacted_journal_relative(&relative, &cached.checkpoint.events)?;
        cache.insert(
            relative,
            CachedTurn {
                journal_event_count: cached.checkpoint.events.len(),
                ..cached
            },
        );
        Ok(())
    }

    fn update<T>(
        &self,
        session_id: &str,
        turn_id: u64,
        mutate: impl FnOnce(&mut DurableTurnCheckpoint) -> Result<T, DurableTurnStoreError>,
    ) -> Result<T, DurableTurnStoreError> {
        self.update_if(session_id, turn_id, |checkpoint| {
            mutate(checkpoint).map(Some)
        })
        .map(|value| value.expect("unconditional mutation returns a value"))
    }

    fn update_if<T>(
        &self,
        session_id: &str,
        turn_id: u64,
        mutate: impl FnOnce(&mut DurableTurnCheckpoint) -> Result<Option<T>, DurableTurnStoreError>,
    ) -> Result<Option<T>, DurableTurnStoreError> {
        let mut cache = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.update_if_locked(&mut cache, session_id, turn_id, mutate)
    }

    fn update_if_locked<T>(
        &self,
        cache: &mut HashMap<PathBuf, CachedTurn>,
        session_id: &str,
        turn_id: u64,
        mutate: impl FnOnce(&mut DurableTurnCheckpoint) -> Result<Option<T>, DurableTurnStoreError>,
    ) -> Result<Option<T>, DurableTurnStoreError> {
        let relative = relative_path(session_id, turn_id)?;
        let exists = self
            .root
            .join(&relative)
            .try_exists()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if !exists {
            return Err(DurableTurnStoreError::NotFound {
                session_id: session_id.to_string(),
                turn_id,
            });
        }

        let cached = self.load_cached_turn(cache, &relative)?.clone();
        let mut checkpoint = cached.checkpoint.clone();
        let Some(value) = mutate(&mut checkpoint)? else {
            return Ok(None);
        };
        checkpoint.reconcile_sequences();
        checkpoint.revision = checkpoint.revision.saturating_add(1);
        self.sync_journal_relative(&relative)?;
        self.write_metadata_relative(&relative, &checkpoint)?;
        cache.insert(
            relative,
            CachedTurn {
                checkpoint,
                journal_event_count: cached.journal_event_count,
            },
        );
        Ok(Some(value))
    }

    fn sync_journal_relative(&self, relative: &Path) -> Result<(), DurableTurnStoreError> {
        let journal = self.root.join(journal_relative_path(relative)?);
        let exists = journal
            .try_exists()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if !exists {
            return Ok(());
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        #[cfg(test)]
        self.journal_sync_count.fetch_add(1, Ordering::Relaxed);
        file.sync_data()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))
    }

    #[cfg(test)]
    fn journal_sync_count(&self) -> usize {
        self.journal_sync_count.load(Ordering::Relaxed)
    }

    fn load_cached_turn<'a>(
        &self,
        cache: &'a mut HashMap<PathBuf, CachedTurn>,
        relative: &Path,
    ) -> Result<&'a mut CachedTurn, DurableTurnStoreError> {
        use std::collections::hash_map::Entry;

        match cache.entry(relative.to_path_buf()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let cached = self.read_relative(relative)?;
                Ok(entry.insert(cached))
            }
        }
    }

    fn read_relative(&self, relative: &Path) -> Result<CachedTurn, DurableTurnStoreError> {
        let body = lingxi_core::host::rooted_fs::read_to_string(&self.root, relative)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        let metadata = serde_json::from_str::<StoredTurnMetadata>(&body)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        let journal_events = self.read_journal_relative(relative)?;
        let journal_event_count = journal_events.len();
        let checkpoint = DurableTurnCheckpoint::from_parts(metadata, journal_events);
        Ok(CachedTurn {
            checkpoint,
            journal_event_count,
        })
    }

    fn write_metadata_relative(
        &self,
        relative: &Path,
        checkpoint: &DurableTurnCheckpoint,
    ) -> Result<(), DurableTurnStoreError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if let Some(parent) = relative.parent() {
            std::fs::create_dir_all(self.root.join(parent))
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        }
        let mut body = serde_json::to_vec_pretty(&checkpoint.stored_metadata())
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        body.push(b'\n');
        lingxi_core::host::rooted_fs::atomic_write(
            &self.root,
            relative,
            &body,
            lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
        )
        .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))
    }

    fn append_journal_relative(
        &self,
        relative: &Path,
        event: &DurableTurnEvent,
        durable: bool,
    ) -> Result<(), DurableTurnStoreError> {
        let journal = self.root.join(journal_relative_path(relative)?);
        if let Some(parent) = journal.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        serde_json::to_writer(&mut file, event)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        file.write_all(b"\n")
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        file.flush()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if durable {
            file.sync_data()
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        }
        Ok(())
    }

    fn write_compacted_journal_relative(
        &self,
        relative: &Path,
        events: &[DurableTurnEvent],
    ) -> Result<(), DurableTurnStoreError> {
        let mut body = Vec::new();
        for event in events {
            serde_json::to_writer(&mut body, event)
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
            body.push(b'\n');
        }
        lingxi_core::host::rooted_fs::atomic_write(
            &self.root,
            &journal_relative_path(relative)?,
            &body,
            lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
        )
        .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))
    }

    fn read_journal_relative(
        &self,
        relative: &Path,
    ) -> Result<Vec<DurableTurnEvent>, DurableTurnStoreError> {
        let journal_relative = journal_relative_path(relative)?;
        let journal = self.root.join(&journal_relative);
        let exists = journal
            .try_exists()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if !exists {
            return Ok(Vec::new());
        }
        // Read bytes rather than UTF-8 text: a torn write can split a
        // multibyte character in the final event body, and that tail must be
        // discarded just like any other incomplete record.
        let body = std::fs::read(&journal)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        // A process can die after writing an event body but before its newline
        // delimiter. Ignore that torn tail for recovery, then truncate it
        // before the next append so a later valid record cannot be glued to
        // the incomplete JSON and make the journal permanently unreadable.
        let valid_len = body
            .split_inclusive(|byte| *byte == b'\n')
            .take_while(|line| line.ends_with(b"\n"))
            .map(<[u8]>::len)
            .sum::<usize>();
        let events = parse_journal_events(&body[..valid_len])?;
        if valid_len < body.len() {
            lingxi_core::host::rooted_fs::atomic_write(
                &self.root,
                &journal_relative,
                &body[..valid_len],
                lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
            )
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        }
        Ok(events)
    }
}

fn join_source_prompts(prompts: &[Utf16JsonProjection]) -> Utf16JsonProjection {
    let mut units = Vec::new();
    for (index, prompt) in prompts.iter().enumerate() {
        if index > 0 {
            units.push(u16::from(b'\n'));
        }
        units.extend(prompt.string_units("").unwrap_or_default());
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("joined source prompt is a valid root string")
}

fn relative_path(session_id: &str, turn_id: u64) -> Result<PathBuf, DurableTurnStoreError> {
    let valid = !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !valid {
        return Err(DurableTurnStoreError::InvalidIdentity(
            session_id.to_string(),
        ));
    }
    Ok(PathBuf::from(session_id).join(format!("{turn_id}.json")))
}

fn journal_relative_path(relative: &Path) -> Result<PathBuf, DurableTurnStoreError> {
    let stem = relative
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| DurableTurnStoreError::Storage("invalid durable turn path".to_string()))?;
    let mut journal_name = stem.to_string();
    journal_name.push_str(".events.jsonl");
    let mut journal = relative.to_path_buf();
    journal.set_file_name(journal_name);
    Ok(journal)
}

fn max_journal_events_before_compaction() -> usize {
    MAX_RETAINED_EVENTS.saturating_add(JOURNAL_COMPACTION_SLACK)
}

fn parse_journal_events(body: &[u8]) -> Result<Vec<DurableTurnEvent>, DurableTurnStoreError> {
    let mut events = Vec::new();
    for line in body.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let json = &line[..line.len().saturating_sub(1)];
        if json.is_empty() {
            continue;
        }
        let event = serde_json::from_slice::<DurableTurnEvent>(json)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DurableTurnStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = DurableTurnStore::new(temp.path().join("turns"));
        (temp, store)
    }

    #[test]
    fn queued_prompts_are_private_ordered_and_claimed_only_at_dispatch() {
        let (temp, store) = store();
        let first = store
            .admit_queued_prompt(
                "session",
                None,
                None,
                Utf16JsonProjection::from("first"),
                Some(PromptModeDto::Normal),
                vec![ImageRefDto {
                    media_type: "image/png".into(),
                    base64: "Zmlyc3Q=".into(),
                }],
            )
            .expect("persist no-id queued prompt");
        let second = store
            .admit_queued_prompt(
                "session",
                Some(41),
                Some(41),
                Utf16JsonProjection::from("second"),
                Some(PromptModeDto::Bash),
                vec![ImageRefDto {
                    media_type: "image/jpeg".into(),
                    base64: "c2Vjb25k".into(),
                }],
            )
            .expect("persist supplied-id queued prompt");
        let duplicate = store
            .admit_queued_prompt(
                "session",
                Some(41),
                Some(41),
                Utf16JsonProjection::from("second"),
                Some(PromptModeDto::Bash),
                vec![ImageRefDto {
                    media_type: "image/jpeg".into(),
                    base64: "c2Vjb25k".into(),
                }],
            )
            .expect("same supplied ID and payload is idempotent");
        assert_eq!(first.turn_id, 1, "the host allocates a resumable ID");
        assert_eq!(first.original_client_turn_id, None);
        assert_eq!(first.queue_sequence, 1);
        assert_eq!(first.state, None, "private Queued is not public Running");
        assert_eq!(second.turn_id, 41);
        assert_eq!(second.original_client_turn_id, Some(41));
        assert_eq!(second.queue_sequence, 2);
        assert_eq!(duplicate.prompt_identity, second.prompt_identity);
        assert_eq!(duplicate.queue_sequence, second.queue_sequence);
        assert!(matches!(
            store.admit_queued_prompt(
                "session",
                Some(41),
                Some(41),
                Utf16JsonProjection::from("changed payload"),
                Some(PromptModeDto::Bash),
                second.images.clone(),
            ),
            Err(DurableTurnStoreError::AlreadyExists { .. })
        ));

        assert_eq!(
            store
                .resume("session", first.turn_id)
                .expect("queued resume"),
            (ResumeDisposition::Queued, None)
        );
        drop(store);

        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let pending = cold.scan_pending("session").expect("scan cold queue");
        assert_eq!(
            pending
                .iter()
                .map(|entry| entry.turn_id)
                .collect::<Vec<_>>(),
            vec![1, 41],
            "scanner preserves durable admission sequence"
        );
        assert_eq!(pending[0].images[0].media_type, "image/png");
        assert_eq!(pending[0].original_client_turn_id, None);
        assert_eq!(pending[0].prompt_identity, first.prompt_identity);
        assert_eq!(pending[1].prompt_mode, Some(PromptModeDto::Bash));
        assert!(pending.iter().all(|entry| entry.state.is_none()));

        let claimed = cold
            .claim_pending("session", first.turn_id)
            .expect("claim only at dispatch");
        assert_eq!(claimed.state, Some(TurnRecoveryStateDto::Running));
        assert_eq!(claimed.queue_delivery, QueueDeliveryState::Claimed);
        assert_eq!(
            cold.scan_pending("session")
                .expect("remaining pending")
                .iter()
                .map(|entry| entry.turn_id)
                .collect::<Vec<_>>(),
            vec![41]
        );
    }

    #[test]
    fn claimed_slash_images_survive_cold_resume_into_prompt_input() {
        let (temp, store) = store();
        let images = vec![ImageRefDto {
            media_type: "image/png".into(),
            base64: "AQID".into(),
        }];
        let admitted = store
            .admit_queued_slash(
                "session",
                Some(47),
                Some(47),
                Utf16JsonProjection::from("/inspect"),
                images.clone(),
            )
            .expect("persist the slash command and images before dispatch");
        store
            .claim_queued_slash("session", admitted.turn_id)
            .expect("claim the non-replayable slash dispatch");
        let prepared = store
            .prepare_claimed_slash_turn(
                "session",
                admitted.turn_id,
                Utf16JsonProjection::from("Describe the attached image"),
                None,
            )
            .expect("make the dispatched command's model turn recoverable");
        assert_eq!(prepared.queue_delivery, QueueDeliveryState::Claimed);
        assert!(prepared.safe_to_resume);

        drop(store);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        assert_eq!(
            cold.resume("session", admitted.turn_id)
                .expect("resume after a process restart")
                .0,
            ResumeDisposition::Ready,
        );
        let restored = cold
            .load("session", admitted.turn_id)
            .expect("reload the recoverable slash turn");
        assert_eq!(restored.images, images);
        let identity = restored
            .prompt_identity
            .as_ref()
            .expect("the RunAsTurn continuation retains its prompt identity");
        let input = identity.queued_prompt_input(restored.prompt.clone(), &restored.images);
        assert_eq!(input.images.len(), 1);
        assert!(matches!(
            &input.images[0],
            ImageSource::Base64 { media_type, data }
                if media_type == "image/png" && data == "AQID"
        ));
    }

    #[test]
    fn claimed_bash_is_at_most_once_and_cold_resume_uses_persisted_result_rows() {
        let (temp, store) = store();
        let admitted = store
            .admit_queued_prompt(
                "session",
                None,
                None,
                Utf16JsonProjection::parse(r#""echo \ud800""#).unwrap(),
                Some(PromptModeDto::Bash),
                vec![ImageRefDto {
                    media_type: "image/png".into(),
                    base64: "aW1hZ2U=".into(),
                }],
            )
            .expect("persist Bash input before dispatch");
        let input_identity = admitted.prompt_identity.clone().unwrap();
        let claimed = store
            .claim_queued_bash("session", admitted.turn_id, false)
            .expect("claim the side-effect boundary");
        assert_eq!(claimed.state, Some(TurnRecoveryStateDto::Running));
        assert_eq!(claimed.queue_delivery, QueueDeliveryState::BashClaimed);
        assert!(!claimed.safe_to_resume);
        assert!(store.scan_pending("session").unwrap().is_empty());
        assert!(claimed.queued_prompt_inputs(&store).is_err());
        assert!(matches!(
            store.claim_queued_bash("session", admitted.turn_id, true),
            Err(DurableTurnStoreError::Terminal { .. })
        ));

        let uncertain_temp = tempfile::tempdir().unwrap();
        let uncertain_store = DurableTurnStore::new(uncertain_temp.path().join("turns"));
        uncertain_store
            .admit_queued_prompt(
                "session",
                Some(99),
                Some(99),
                Utf16JsonProjection::from("touch uncertain"),
                Some(PromptModeDto::Bash),
                Vec::new(),
            )
            .unwrap();
        uncertain_store
            .claim_queued_bash("session", 99, true)
            .unwrap();
        drop(uncertain_store);
        let cold_uncertain = DurableTurnStore::new(uncertain_temp.path().join("turns"));
        assert_eq!(
            cold_uncertain.resume("session", 99).unwrap().0,
            ResumeDisposition::WaitingForUser,
            "a crash while Bash is in flight must never auto-run it again"
        );
        assert!(cold_uncertain.scan_pending("session").unwrap().is_empty());

        let result = DurableBashResult {
            stdout: "stdout ✓".into(),
            stderr: "stderr".into(),
            exit_code: 7,
            interrupted: false,
        };
        let prepared = store
            .prepare_claimed_bash_turn("session", admitted.turn_id, result.clone(), false)
            .expect("atomically persist the result before model follow-up");
        assert_eq!(prepared.queue_delivery, QueueDeliveryState::Claimed);
        assert!(prepared.safe_to_resume);
        let result_row_id = prepared.bash_result_row_id.unwrap();
        let input_units = prepared.prompt.string_units("").unwrap();
        let mut expected_input = "<bash-input>".encode_utf16().collect::<Vec<_>>();
        expected_input.extend("echo ".encode_utf16());
        expected_input.push(0xd800);
        expected_input.extend("</bash-input>".encode_utf16());
        assert_eq!(input_units, expected_input);

        drop(store);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let restored = cold.load("session", admitted.turn_id).unwrap();
        assert_eq!(restored.bash_result, Some(result));
        assert_eq!(restored.bash_should_query, Some(false));
        assert_eq!(restored.bash_result_row_id, Some(result_row_id));
        let inputs = restored.queued_prompt_inputs(&cold).unwrap();
        assert_eq!(
            inputs.len(),
            2,
            "cold false-response replay includes its notice"
        );
        assert!(inputs.iter().all(|input| input.skip_model_query));
        assert!(inputs[0].is_meta);
        assert_eq!(
            inputs[0].text.string_at(""),
            Some("<system-reminder>The command below was run directly in Claude Code, not sent to you as a request, and its output goes straight to the user. It's recorded here as context for later messages.</system-reminder>")
        );
        let bash_input = &inputs[1];
        assert_eq!(bash_input.text.string_units(""), Some(expected_input));
        assert_eq!(bash_input.message_id, Some(input_identity.message_id));
        assert_eq!(
            bash_input.prompt_ingress_kind,
            Some(orchestrator::PromptIngressKind::DirectBash),
            "cold recovery preserves the host-stamped Bash follow-up source"
        );
        let pending_identity = bash_input.pending_prompt_identity.as_ref().unwrap();
        assert_eq!(pending_identity.prompt_id, input_identity.prompt_id);
        assert_eq!(pending_identity.trailing_history_row_ids, [result_row_id]);
        assert_eq!(bash_input.trailing_history_rows.len(), 1);
        assert_eq!(
            bash_input.trailing_history_rows[0].string_at(""),
            Some("<bash-stdout>stdout ✓</bash-stdout><bash-stderr>stderr</bash-stderr>")
        );
        assert!(matches!(
            &bash_input.images[0],
            ImageSource::Base64 { media_type, data }
                if media_type == "image/png" && data == "aW1hZ2U="
        ));
        assert_eq!(cold.scan_pending("session").unwrap().len(), 0);
    }

    #[test]
    fn coalesced_mobile_batch_replays_last_member_identity_and_ordered_images() {
        let (temp, store) = store();
        let first = store
            .admit_queued_prompt(
                "session",
                Some(11),
                Some(11),
                Utf16JsonProjection::from("first"),
                None,
                vec![ImageRefDto {
                    media_type: "image/png".into(),
                    base64: "Zmlyc3Q=".into(),
                }],
            )
            .expect("first prompt");
        let second = store
            .admit_queued_prompt(
                "session",
                Some(12),
                Some(12),
                Utf16JsonProjection::from("second"),
                None,
                vec![ImageRefDto {
                    media_type: "image/jpeg".into(),
                    base64: "c2Vjb25k".into(),
                }],
            )
            .expect("second prompt");
        let first_identity = first.prompt_identity.clone().unwrap();
        let last_identity = second.prompt_identity.clone().unwrap();
        assert_ne!(
            first_identity.message_id, last_identity.message_id,
            "coalesced members have distinct stable Native UUIDs"
        );
        let batch = store
            .prepare_mid_turn_batch("session", &[first.turn_id, second.turn_id])
            .expect("persist aggregate before cache acknowledgement");
        assert_eq!(batch.owner_turn_id, first.turn_id);
        assert_eq!(batch.aggregate_identity, last_identity);
        assert_eq!(
            batch
                .member_identities
                .iter()
                .map(|identity| identity.turn_id)
                .collect::<Vec<_>>(),
            [first.turn_id, second.turn_id],
            "the owner envelope retains every source identity in member order"
        );
        assert_eq!(
            batch
                .member_identities
                .iter()
                .map(|identity| identity.prompt_identity.clone())
                .collect::<Vec<_>>(),
            [first_identity, last_identity],
            "the owner envelope preserves each member UUID and prompt ID in order"
        );
        drop(store);

        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let pending = cold.scan_pending("session").expect("restore pending batch");
        assert_eq!(pending.len(), 1, "member rows do not replay independently");
        assert_eq!(pending[0].turn_id, first.turn_id);
        assert_eq!(
            pending[0].queue_delivery,
            QueueDeliveryState::CoalescedClaimed
        );
        assert_eq!(pending[0].coalesced_batch, Some(batch.clone()));
        let inputs = pending[0]
            .queued_prompt_inputs(&cold)
            .expect("reconstruct aggregate input");
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].text.string_at(""), Some("first\nsecond"));
        assert_eq!(
            inputs[0].message_id,
            Some(batch.aggregate_identity.message_id)
        );
        assert_eq!(
            inputs[0]
                .pending_prompt_identity
                .as_ref()
                .map(|identity| identity.prompt_id.as_str()),
            Some(batch.aggregate_identity.prompt_id.as_str())
        );
        assert_eq!(inputs[0].images.len(), 2);
        assert!(matches!(
            &inputs[0].images[0],
            lingxi_core::types::ImageSource::Base64 { media_type, data }
                if media_type == "image/png" && data == "Zmlyc3Q="
        ));
        assert!(matches!(
            &inputs[0].images[1],
            lingxi_core::types::ImageSource::Base64 { media_type, data }
                if media_type == "image/jpeg" && data == "c2Vjb25k"
        ));

        cold.acknowledge_mid_turn_batch("session", first.turn_id, &[first.turn_id, second.turn_id])
            .expect("acknowledge after append-once writer succeeds");
        let acknowledged = cold
            .load("session", first.turn_id)
            .expect("read aggregate commit marker");
        assert_eq!(acknowledged.queue_delivery, QueueDeliveryState::Delivered);
        assert_eq!(
            acknowledged.coalesced_batch,
            Some(batch),
            "the ACK keeps the ordered source identities available for audit"
        );
        assert!(cold
            .scan_pending("session")
            .expect("batch is terminal after acknowledgment")
            .is_empty());
    }

    #[test]
    fn durable_mid_turn_claim_rejects_mixed_or_multiple_bash_members() {
        let (_temp, store) = store();
        let normal = store
            .admit_queued_prompt(
                "session",
                None,
                None,
                Utf16JsonProjection::from("normal"),
                Some(PromptModeDto::Normal),
                Vec::new(),
            )
            .expect("normal prompt");
        let bash_a = store
            .admit_queued_prompt(
                "session",
                None,
                None,
                Utf16JsonProjection::from("echo a"),
                Some(PromptModeDto::Bash),
                Vec::new(),
            )
            .expect("first Bash command");
        let bash_b = store
            .admit_queued_prompt(
                "session",
                None,
                None,
                Utf16JsonProjection::from("echo b"),
                Some(PromptModeDto::Bash),
                Vec::new(),
            )
            .expect("second Bash command");

        assert!(matches!(
            store.prepare_mid_turn_batch("session", &[normal.turn_id, bash_a.turn_id]),
            Err(DurableTurnStoreError::Storage(message))
                if message.contains("Bash execution boundary")
        ));
        assert!(matches!(
            store.prepare_mid_turn_batch("session", &[bash_a.turn_id, bash_b.turn_id]),
            Err(DurableTurnStoreError::Storage(message))
                if message.contains("Bash execution boundary")
        ));
        let pending = store.scan_pending("session").expect("scan pending prompts");
        assert_eq!(
            pending
                .iter()
                .map(|entry| entry.turn_id)
                .collect::<Vec<_>>(),
            [normal.turn_id, bash_a.turn_id, bash_b.turn_id],
            "rejected mixed claims leave every source dispatchable"
        );
        assert!(pending.iter().all(|entry| entry.coalesced_batch.is_none()));
    }

    #[test]
    fn concurrent_mid_turn_admission_and_cancel_have_one_ordered_owner() {
        let (_temp, store) = store();
        let first = store
            .admit_queued_prompt(
                "session",
                Some(51),
                Some(51),
                Utf16JsonProjection::from("first"),
                None,
                Vec::new(),
            )
            .expect("first prompt");
        let second = store
            .admit_queued_prompt(
                "session",
                Some(52),
                Some(52),
                Utf16JsonProjection::from("second"),
                None,
                Vec::new(),
            )
            .expect("second prompt");
        let first_id = first.turn_id;
        let second_id = second.turn_id;
        let store = std::sync::Arc::new(store);
        let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
        let prepare_store = store.clone();
        let prepare_gate = gate.clone();
        let prepare = std::thread::spawn(move || {
            prepare_gate.wait();
            prepare_store.prepare_mid_turn_batch("session", &[first_id, second_id])
        });
        let cancel_store = store.clone();
        let cancel_gate = gate.clone();
        let cancel = std::thread::spawn(move || {
            cancel_gate.wait();
            cancel_store.cancel_pending("session", second_id)
        });
        gate.wait();

        let prepared = prepare.join().expect("prepare worker");
        let cancelled = cancel.join().expect("cancel worker");
        match prepared {
            Ok(batch) => {
                assert_eq!(batch.member_turn_ids, [first_id, second_id]);
                assert_eq!(
                    cancelled.expect("cancel result"),
                    None,
                    "claim admission wins atomically; cancellation must route to the active turn"
                );
                let pending = store.scan_pending("session").expect("scan claimed group");
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].turn_id, first_id);
                assert_eq!(
                    pending[0].queue_delivery,
                    QueueDeliveryState::CoalescedClaimed
                );
            }
            Err(DurableTurnStoreError::Terminal { .. }) => {
                assert_eq!(
                    cancelled.expect("cancel result"),
                    Some(vec![second_id]),
                    "cancellation before admission removes only its unbatched source"
                );
                assert_eq!(
                    store
                        .scan_pending("session")
                        .expect("scan surviving prefix")
                        .iter()
                        .map(|checkpoint| checkpoint.turn_id)
                        .collect::<Vec<_>>(),
                    [first_id],
                    "a canceled member cannot leak into a later aggregate"
                );
            }
            Err(error) => panic!("unexpected batch admission result: {error}"),
        }
    }

    #[test]
    fn cancelling_private_queue_entry_does_not_publish_recovery_state() {
        let (_temp, store) = store();
        let queued = store
            .admit_queued_prompt(
                "session",
                Some(18),
                Some(18),
                Utf16JsonProjection::from("cancel me"),
                None,
                Vec::new(),
            )
            .expect("persist queued prompt");
        assert_eq!(
            store
                .cancel("session", queued.turn_id)
                .expect("cancel queued"),
            None,
            "an unclaimed prompt has no public recovery snapshot"
        );
        let cancelled = store
            .load("session", queued.turn_id)
            .expect("load cancelled");
        assert_eq!(cancelled.state, None);
        assert_eq!(cancelled.queue_delivery, QueueDeliveryState::Cancelled);
        assert!(store
            .scan_pending("session")
            .expect("scan cancelled prompt")
            .is_empty());
    }

    #[test]
    fn cancelling_a_member_after_mid_turn_admission_preserves_claimed_batch() {
        let (_temp, store) = store();
        let first = store
            .admit_queued_prompt(
                "session",
                Some(21),
                Some(21),
                Utf16JsonProjection::from("first"),
                None,
                Vec::new(),
            )
            .expect("first prompt");
        let second = store
            .admit_queued_prompt(
                "session",
                Some(22),
                Some(22),
                Utf16JsonProjection::from("second"),
                None,
                Vec::new(),
            )
            .expect("second prompt");
        store
            .prepare_mid_turn_batch("session", &[first.turn_id, second.turn_id])
            .expect("prepare batch");

        assert_eq!(
            store
                .cancel_pending("session", second.turn_id)
                .expect("cancel after admission"),
            None,
            "admitted batches are canceled by unwinding the active turn"
        );
        let pending = store
            .scan_pending("session")
            .expect("claimed batch remains recoverable");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].turn_id, first.turn_id);
        assert_eq!(
            pending[0].queue_delivery,
            QueueDeliveryState::CoalescedClaimed
        );
    }

    #[test]
    fn cold_restart_replays_the_exact_durable_prompt_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("turns");
        let prompt_units = "recover "
            .encode_utf16()
            .chain([0xd800])
            .collect::<Vec<_>>();
        let prompt = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&prompt_units),
            prompt_units.clone(),
        )
        .expect("source prompt units are valid");
        let store = DurableTurnStore::new(root.clone());
        store
            .begin("session", 42, prompt, None, Vec::new())
            .expect("begin turn before invoking the model");

        let metadata_path = root.join("session/42.json");
        let metadata = std::fs::read_to_string(&metadata_path).expect("read durable metadata");
        assert!(
            metadata.contains(r#""prompt": "recover \ud800""#),
            "the prompt must be written as its original JSON string value: {metadata}"
        );
        assert!(
            !metadata.contains("Utf16JsonProjection"),
            "the storage envelope must not serialize the carrier implementation"
        );

        drop(store);
        let restarted = DurableTurnStore::new(root);
        let checkpoint = restarted.load("session", 42).expect("cold-load checkpoint");
        assert_eq!(
            checkpoint.prompt.string_units(""),
            Some(prompt_units.clone())
        );
        assert_eq!(checkpoint.prompt.string_at(""), Some("recover �"));

        let (disposition, snapshot) = restarted
            .resume("session", 42)
            .expect("resume exact checkpoint");
        assert_eq!(disposition, ResumeDisposition::Ready);
        let snapshot = snapshot.expect("claimed turn has a public snapshot");
        assert_eq!(snapshot.state, TurnRecoveryStateDto::Running);
        let resumed = restarted
            .load("session", 42)
            .expect("load resumed checkpoint");
        assert_eq!(resumed.prompt.string_units(""), Some(prompt_units));
    }

    #[test]
    fn cold_restart_preserves_every_source_projection_in_skill_prompt_batch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("turns");
        let command_units = "/review "
            .encode_utf16()
            .chain([0xd800])
            .collect::<Vec<_>>();
        let body_units = "body 🚀 "
            .encode_utf16()
            .chain([0xdc00])
            .collect::<Vec<_>>();
        let command = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&command_units),
            command_units.clone(),
        )
        .expect("command projection");
        let body = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&body_units),
            body_units.clone(),
        )
        .expect("body projection");
        let hook = Utf16JsonProjection::parse(
            r#"{"role":"user","content":[{"type":"text","text":"hook \ud800"}]}"#,
        )
        .expect("hook row projection");
        let later_hook = Utf16JsonProjection::parse(
            r#"{"role":"assistant","content":[{"type":"text","text":"later hook"}]}"#,
        )
        .expect("later hook row projection");
        let skill_prompt =
            DurableSkillPrompt::new(command.clone(), body.clone(), vec![hook, later_hook]);
        let input_message_ids = skill_prompt.input_message_ids;
        let hook_message_ids = skill_prompt.hook_message_ids.clone();
        let input_prompt_ids = skill_prompt.input_prompt_ids.clone();

        let store = DurableTurnStore::new(root.clone());
        store
            .begin_with_skill_prompt(
                "session",
                43,
                command.clone(),
                None,
                Vec::new(),
                Some(skill_prompt),
            )
            .expect("persist source-backed skill batch before model execution");
        let metadata_path = root.join("session/43.json");
        let metadata = std::fs::read_to_string(&metadata_path).expect("read checkpoint");
        assert!(
            !metadata.contains("Utf16JsonProjection"),
            "checkpoint stores raw source JSON values rather than carrier internals"
        );
        let stored_source =
            Utf16JsonProjection::parse(&metadata).expect("durability metadata remains source JSON");
        let hook_units = "hook ".encode_utf16().chain([0xd800]).collect::<Vec<_>>();
        let later_hook_units = "later hook".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            stored_source.string_units("/skill_prompt/command"),
            Some(command_units.clone())
        );
        assert_eq!(
            stored_source.string_units("/skill_prompt/body"),
            Some(body_units.clone())
        );
        assert_eq!(
            stored_source.string_units("/skill_prompt/hook_messages/0/content/0/text"),
            Some(hook_units.clone())
        );
        assert_eq!(
            stored_source.string_units("/skill_prompt/hook_messages/1/content/0/text"),
            Some(later_hook_units.clone())
        );
        for (index, message_id) in input_message_ids.iter().enumerate() {
            let message_id_text = message_id.as_uuid().to_string();
            assert_eq!(
                stored_source.string_at(&format!("/skill_prompt/input_message_ids/{index}")),
                Some(message_id_text.as_str())
            );
            assert_eq!(
                stored_source.string_at(&format!("/skill_prompt/input_prompt_ids/{index}")),
                Some(input_prompt_ids[index].as_str())
            );
        }
        for (index, message_id) in hook_message_ids.iter().enumerate() {
            let message_id_text = message_id.as_uuid().to_string();
            assert_eq!(
                stored_source.string_at(&format!("/skill_prompt/hook_message_ids/{index}")),
                Some(message_id_text.as_str())
            );
        }

        drop(store);
        let restarted = DurableTurnStore::new(root);
        let checkpoint = restarted
            .load("session", 43)
            .expect("cold-load complete skill prompt batch");
        let restored = checkpoint
            .skill_prompt
            .as_ref()
            .expect("skill batch survives process restart");
        assert_eq!(restored.command.string_units(""), Some(command_units));
        assert_eq!(restored.body.string_units(""), Some(body_units));
        assert_eq!(restored.input_message_ids, input_message_ids);
        assert_eq!(restored.hook_message_ids, hook_message_ids);
        assert_eq!(restored.input_prompt_ids, input_prompt_ids);
        let queued_inputs = restored.queued_prompt_inputs(&[]);
        assert_eq!(queued_inputs.len(), 2);
        assert_eq!(
            queued_inputs[0].message_id,
            Some(input_message_ids[0]),
            "cold reconstruction reuses the command row identity"
        );
        assert_eq!(
            queued_inputs[1].message_id,
            Some(input_message_ids[1]),
            "cold reconstruction reuses the body row identity"
        );
        assert_eq!(
            queued_inputs[0]
                .pending_prompt_identity
                .as_ref()
                .expect("command carries durable prompt identity")
                .prompt_id,
            input_prompt_ids[0]
        );
        assert_eq!(
            queued_inputs[1]
                .pending_prompt_identity
                .as_ref()
                .expect("body carries durable prompt identity")
                .prompt_id,
            input_prompt_ids[1]
        );
        assert_eq!(
            queued_inputs[0]
                .pending_prompt_identity
                .as_ref()
                .expect("command row has stable checkpointed identity")
                .trailing_history_row_ids,
            Vec::<MessageId>::new(),
            "command input has no trailing hook rows"
        );
        assert_eq!(
            queued_inputs[1]
                .pending_prompt_identity
                .as_ref()
                .expect("body row has stable checkpointed identity")
                .trailing_history_row_ids,
            hook_message_ids,
            "hook row IDs remain paired with their source input order"
        );
        assert_eq!(
            restored.hook_messages[0].string_units("/content/0/text"),
            Some(hook_units.clone())
        );
        assert_eq!(
            restored.hook_messages[1].string_units("/content/0/text"),
            Some(later_hook_units.clone())
        );

        let (disposition, _) = restarted
            .resume("session", 43)
            .expect("resume from source-preserving batch checkpoint");
        assert_eq!(disposition, ResumeDisposition::Ready);
        let resumed = restarted
            .load("session", 43)
            .expect("load resumed batch checkpoint");
        assert_eq!(
            resumed
                .skill_prompt
                .as_ref()
                .expect("batch is retained while running")
                .hook_messages[0]
                .string_units("/content/0/text"),
            Some(hook_units)
        );
        assert_eq!(
            resumed
                .skill_prompt
                .as_ref()
                .expect("batch is retained while running")
                .hook_messages[1]
                .string_units("/content/0/text"),
            Some(later_hook_units)
        );
    }

    #[test]
    fn live_observers_preserve_a_paused_checkpoint_until_explicit_resume() {
        let (_temp, store) = store();
        store
            .begin("session", 1, "prompt".into(), None, vec![])
            .unwrap();
        store
            .transition(
                "session",
                1,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("background_time_expired".into()),
            )
            .unwrap();
        let paused = store.load("session", 1).unwrap();
        for state in [
            TurnRecoveryStateDto::WaitingForUser,
            TurnRecoveryStateDto::Running,
            TurnRecoveryStateDto::Completed,
        ] {
            assert!(store
                .transition_if_live("session", 1, state, false, Some("late observer".into()))
                .unwrap()
                .is_none());
            let unchanged = store.load("session", 1).unwrap();
            assert_eq!(unchanged.state, paused.state);
            assert_eq!(unchanged.revision, paused.revision);
            assert_eq!(unchanged.reason, paused.reason);
            assert_eq!(unchanged.safe_to_resume, paused.safe_to_resume);
        }
        store
            .transition("session", 1, TurnRecoveryStateDto::Running, true, None)
            .unwrap();
        assert_eq!(
            store
                .transition_if_live(
                    "session",
                    1,
                    TurnRecoveryStateDto::WaitingForUser,
                    false,
                    None
                )
                .unwrap()
                .unwrap()
                .state,
            TurnRecoveryStateDto::WaitingForUser
        );
    }

    #[test]
    fn recovery_merges_metadata_prefix_with_journal_suffix() {
        let metadata = StoredTurnMetadata {
            session_id: "session-a".to_string(),
            turn_id: 6,
            prompt: Utf16JsonProjection::from("hello"),
            prompt_identity: None,
            skill_prompt: None,
            prompt_mode: None,
            images: Vec::new(),
            queue_sequence: 0,
            queue_operation: QueueOperationKind::Prompt,
            bash_result: None,
            bash_result_row_id: None,
            bash_notice_identity: None,
            bash_should_query: None,
            original_client_turn_id: Some(6),
            queue_delivery: QueueDeliveryState::Claimed,
            coalesced_batch: None,
            revision: 2,
            state: Some(TurnRecoveryStateDto::Running),
            first_sequence: 1,
            last_sequence: 1,
            safe_to_resume: true,
            reason: None,
            events: vec![DurableTurnEvent {
                sequence: 1,
                event_json: r#"{"type":"turn_started"}"#.to_string(),
            }],
        };
        let checkpoint = DurableTurnCheckpoint::from_parts(
            metadata,
            vec![DurableTurnEvent {
                sequence: 2,
                event_json: r#"{"type":"text_delta"}"#.to_string(),
            }],
        );

        assert_eq!(
            checkpoint
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(checkpoint.first_sequence, 1);
        assert_eq!(checkpoint.last_sequence, 2);
    }

    #[test]
    fn checkpoint_is_atomic_and_replays_strictly_after_cursor() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                7,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                7,
                r#"{"type":"text_delta","text":"a"}"#.to_string(),
            )
            .expect("event one");
        store
            .append_event(
                "session-a",
                7,
                r#"{"type":"text_delta","text":"b"}"#.to_string(),
            )
            .expect("event two");

        let checkpoint = store.load("session-a", 7).expect("load");
        assert_eq!(checkpoint.snapshot().last_sequence, 2);
        assert_eq!(checkpoint.replay_after(Some(1)).len(), 1);
        assert_eq!(checkpoint.replay_after(Some(1))[0].sequence, 2);
    }

    #[test]
    fn normal_appends_leave_metadata_unchanged_until_a_transition() {
        let (temp, store) = store();
        store
            .begin(
                "session-a",
                71,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                71,
                r#"{"type":"text_delta","text":"a"}"#.to_string(),
            )
            .expect("event one");
        store
            .append_event(
                "session-a",
                71,
                r#"{"type":"text_delta","text":"b"}"#.to_string(),
            )
            .expect("event two");

        let metadata_body = std::fs::read_to_string(temp.path().join("turns/session-a/71.json"))
            .expect("read metadata");
        let metadata =
            serde_json::from_str::<StoredTurnMetadata>(&metadata_body).expect("parse metadata");
        assert_eq!(metadata.last_sequence, 0);
        assert_eq!(metadata.revision, 1);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("turns/session-a/71.events.jsonl"))
                .expect("read journal")
                .lines()
                .count(),
            2
        );

        let transitioned = store
            .transition(
                "session-a",
                71,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                Some("backgrounded".to_string()),
            )
            .expect("transition");
        assert_eq!(transitioned.last_sequence, 2);
        let metadata_body = std::fs::read_to_string(temp.path().join("turns/session-a/71.json"))
            .expect("read transitioned metadata");
        let metadata = serde_json::from_str::<StoredTurnMetadata>(&metadata_body)
            .expect("parse transitioned metadata");
        assert_eq!(metadata.last_sequence, 2);
        assert_eq!(metadata.revision, 4);
    }

    #[test]
    fn metadata_transition_syncs_journal_after_unsynced_deltas() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                711,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                711,
                r#"{"type":"text_delta","text":"a"}"#.to_string(),
            )
            .expect("event");
        assert_eq!(store.journal_sync_count(), 0);

        store
            .transition(
                "session-a",
                711,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                Some("backgrounded".to_string()),
            )
            .expect("transition");
        assert_eq!(store.journal_sync_count(), 1);
    }

    #[test]
    fn cold_recovery_derives_terminal_state_from_journal_before_metadata_transition() {
        let (temp, store) = store();
        store
            .begin(
                "session-a",
                72,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        // Simulate a process dying after the terminal event was committed but
        // before TurnLifecycleListener could persist its metadata transition.
        store
            .append_event(
                "session-a",
                72,
                r#"{"type":"turn_ended","outcome":{"type":"end_turn"}}"#.to_string(),
            )
            .expect("terminal event");

        let recovered_store = DurableTurnStore::new(temp.path().join("turns"));
        let recovered = recovered_store.load("session-a", 72).expect("cold load");
        assert_eq!(recovered.state, Some(TurnRecoveryStateDto::Completed));
        assert!(!recovered.safe_to_resume);
        assert_eq!(
            recovered_store
                .resume("session-a", 72)
                .expect("resume gate")
                .0,
            ResumeDisposition::Terminal
        );
    }

    #[test]
    fn cold_recovery_derives_failed_state_and_reason_from_error_event() {
        let (temp, store) = store();
        store
            .begin(
                "session-a",
                721,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                721,
                r#"{"type":"error","kind":{"type":"internal"},"message":"provider failed"}"#
                    .to_string(),
            )
            .expect("error event");

        let recovered = DurableTurnStore::new(temp.path().join("turns"))
            .load("session-a", 721)
            .expect("cold load");
        assert_eq!(recovered.state, Some(TurnRecoveryStateDto::Failed));
        assert_eq!(recovered.reason.as_deref(), Some("provider failed"));
        assert!(!recovered.safe_to_resume);
    }

    #[test]
    fn explicit_cancel_is_terminal_and_cannot_resume_or_append() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                8,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        let cancelled = store
            .cancel("session-a", 8)
            .expect("cancel")
            .expect("running turn has a public snapshot");
        assert_eq!(cancelled.state, TurnRecoveryStateDto::Cancelled);
        assert!(!cancelled.safe_to_resume);
        assert_eq!(
            store.resume("session-a", 8).expect("resume gate").0,
            ResumeDisposition::Terminal
        );
        assert!(matches!(
            store.append_event("session-a", 8, "{}".to_string()),
            Err(DurableTurnStoreError::Terminal { .. })
        ));
    }

    #[test]
    fn unsafe_checkpoint_waits_for_user_instead_of_replaying() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                9,
                Utf16JsonProjection::from("deploy"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .transition(
                "session-a",
                9,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                Some("external_side_effect_unconfirmed".to_string()),
            )
            .expect("pause");

        let (disposition, snapshot) = store.resume("session-a", 9).expect("resume");
        assert_eq!(disposition, ResumeDisposition::WaitingForUser);
        let snapshot = snapshot.expect("claimed turn has a public snapshot");
        assert_eq!(snapshot.state, TurnRecoveryStateDto::WaitingForUser);
        assert!(!snapshot.safe_to_resume);
    }

    #[test]
    fn safe_checkpoint_resumes_running() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                10,
                Utf16JsonProjection::from("read"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .transition(
                "session-a",
                10,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("process_restarted".to_string()),
            )
            .expect("pause");

        let (disposition, snapshot) = store.resume("session-a", 10).expect("resume");
        assert_eq!(disposition, ResumeDisposition::Ready);
        let snapshot = snapshot.expect("claimed turn has a public snapshot");
        assert_eq!(snapshot.state, TurnRecoveryStateDto::Running);
        assert!(snapshot.safe_to_resume);
        assert!(snapshot.reason.is_none());
    }

    #[test]
    fn replayed_output_requires_user_instead_of_rerunning_the_prompt() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                11,
                Utf16JsonProjection::from("write"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                11,
                r#"{"type":"text_delta","text":"partial"}"#.to_string(),
            )
            .expect("append");
        store
            .transition(
                "session-a",
                11,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("process_restarted".to_string()),
            )
            .expect("pause");

        let (disposition, snapshot) = store.resume("session-a", 11).expect("resume");
        assert_eq!(disposition, ResumeDisposition::WaitingForUser);
        let snapshot = snapshot.expect("claimed turn has a public snapshot");
        assert_eq!(snapshot.state, TurnRecoveryStateDto::WaitingForUser);
        assert!(!snapshot.safe_to_resume);
    }

    #[test]
    fn turn_started_checkpoint_still_resumes_running() {
        let (_temp, store) = store();
        store
            .begin(
                "session-a",
                12,
                Utf16JsonProjection::from("read"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                12,
                r#"{"type":"turn_started","turn_id":12}"#.to_string(),
            )
            .expect("append");
        store
            .transition(
                "session-a",
                12,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("process_restarted".to_string()),
            )
            .expect("pause");

        let (disposition, snapshot) = store.resume("session-a", 12).expect("resume");
        assert_eq!(disposition, ResumeDisposition::Ready);
        let snapshot = snapshot.expect("claimed turn has a public snapshot");
        assert_eq!(snapshot.state, TurnRecoveryStateDto::Running);
        assert!(snapshot.safe_to_resume);
        assert!(snapshot.reason.is_none());
    }

    #[test]
    fn replay_gap_is_detected_before_replaying_a_truncated_suffix() {
        let mut events = (1..=(MAX_RETAINED_EVENTS as u64 + 1))
            .map(|sequence| DurableTurnEvent {
                sequence,
                event_json: format!(r#"{{"type":"text_delta","text":"{sequence}"}}"#),
            })
            .collect::<Vec<_>>();
        retain_recent_events(&mut events);
        let checkpoint = DurableTurnCheckpoint {
            session_id: "session-a".to_string(),
            turn_id: 13,
            prompt: Utf16JsonProjection::from("gap"),
            prompt_identity: None,
            skill_prompt: None,
            prompt_mode: None,
            images: Vec::new(),
            queue_sequence: 0,
            queue_operation: QueueOperationKind::Prompt,
            bash_result: None,
            bash_result_row_id: None,
            bash_notice_identity: None,
            bash_should_query: None,
            original_client_turn_id: Some(13),
            queue_delivery: QueueDeliveryState::Claimed,
            coalesced_batch: None,
            revision: 1,
            state: Some(TurnRecoveryStateDto::Running),
            first_sequence: events.first().expect("retained event").sequence,
            last_sequence: events.last().expect("retained event").sequence,
            safe_to_resume: true,
            reason: None,
            events,
        };

        assert_eq!(checkpoint.first_sequence, 2);
        assert!(checkpoint.has_replay_gap(None));
        assert!(checkpoint.has_replay_gap(Some(0)));
        assert!(!checkpoint.has_replay_gap(Some(1)));
        assert_eq!(
            checkpoint
                .replay_after(Some(1))
                .first()
                .map(|event| event.sequence),
            Some(2)
        );
    }

    #[test]
    fn load_ignores_a_torn_journal_tail() {
        let (temp, store) = store();
        store
            .begin(
                "session-a",
                14,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                14,
                r#"{"type":"text_delta","text":"stable"}"#.to_string(),
            )
            .expect("append");

        let journal = temp.path().join("turns/session-a/14.events.jsonl");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal");
        file.write_all(br#"{"sequence":2,"event_json":"unterminated"#)
            .expect("write torn tail");
        file.write_all(&[0xff])
            .expect("write a torn multibyte tail");

        let recovered = DurableTurnStore::new(temp.path().join("turns"))
            .load("session-a", 14)
            .expect("recover");
        assert_eq!(recovered.last_sequence, 1);
        assert_eq!(recovered.events.len(), 1);
        assert_eq!(recovered.events[0].sequence, 1);
    }

    #[test]
    fn append_after_recovery_discards_torn_tail_and_keeps_order() {
        let (temp, store) = store();
        store
            .begin(
                "session-a",
                141,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");
        store
            .append_event(
                "session-a",
                141,
                r#"{"type":"text_delta","text":"stable"}"#.to_string(),
            )
            .expect("append stable event");

        let journal = temp.path().join("turns/session-a/141.events.jsonl");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal");
        file.write_all(br#"{"sequence":2,"event_json":"unterminated"#)
            .expect("write torn tail");
        file.write_all(&[0xff])
            .expect("write a torn multibyte tail");

        let recovered_store = DurableTurnStore::new(temp.path().join("turns"));
        recovered_store.load("session-a", 141).expect("recover");
        recovered_store
            .append_event(
                "session-a",
                141,
                r#"{"type":"text_delta","text":"after-recovery"}"#.to_string(),
            )
            .expect("append after recovery");

        let recovered = recovered_store.load("session-a", 141).expect("reload");
        assert_eq!(
            recovered
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(recovered.events[1].event_json.contains("after-recovery"));
    }

    #[test]
    fn compaction_bounds_the_journal_and_keeps_metadata_small() {
        let (temp, store) = store();
        store
            .begin(
                "session-a",
                15,
                Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin");

        let total_events = max_journal_events_before_compaction() + 1;
        for sequence in 0..total_events {
            store
                .append_event(
                    "session-a",
                    15,
                    format!(r#"{{"type":"text_delta","text":"{sequence}"}}"#),
                )
                .expect("append");
        }

        let journal = std::fs::read_to_string(temp.path().join("turns/session-a/15.events.jsonl"))
            .expect("read journal");
        let metadata = std::fs::read_to_string(temp.path().join("turns/session-a/15.json"))
            .expect("read metadata");
        let checkpoint = store.load("session-a", 15).expect("load");

        assert_eq!(journal.lines().count(), MAX_RETAINED_EVENTS);
        assert!(
            !metadata.contains("event_json"),
            "metadata should no longer embed retained events"
        );
        assert_eq!(checkpoint.events.len(), MAX_RETAINED_EVENTS);
        assert_eq!(
            checkpoint.first_sequence,
            (total_events - MAX_RETAINED_EVENTS + 1) as u64
        );
        assert_eq!(checkpoint.last_sequence, total_events as u64);
    }
}
