//! Crash-safe durable metadata for ordinary mobile conversation turns.
//!
//! This store deliberately does not claim arbitrary tool replay is exactly
//! once. It persists the user input, ordered client events, and a conservative
//! recovery gate. A caller may resume automatically only while
//! `safe_to_resume` is true; permission-bound or side-effectful work must park
//! as `WaitingForUser`.

use client::protocol::commands::{ImageRefDto, PromptModeDto};
use client::protocol::events::{
    QueuedTurnStatusDto, TurnRecoverySnapshotDto, TurnRecoveryStateDto,
};
use client::protocol::text::ClientTextDto;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ImageSource, MessageId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

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
    /// The joined row crossed in-turn admission. It may have been appended or
    /// processed, so cold recovery must not automatically dispatch it again.
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
    /// Set only when cancellation atomically won before a private queue claim.
    pub(crate) cancelled_before_claim: bool,
    pub(crate) coalesced_batch: Option<CoalescedPromptBatch>,
    pub(crate) revision: u64,
    pub(crate) state: Option<TurnRecoveryStateDto>,
    pub(crate) first_sequence: u64,
    pub(crate) last_sequence: u64,
    pub(crate) safe_to_resume: bool,
    /// True when the live projection may include content not recoverable from
    /// this journal. This monotonic marker is independent of state/reason.
    pub(crate) projection_uncertain: bool,
    pub(crate) reason: Option<String>,
    pub(crate) events: Vec<DurableTurnEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    cancelled_before_claim: bool,
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
    #[serde(default = "unknown_projection_uncertain")]
    projection_uncertain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    events: Vec<DurableTurnEvent>,
}

// Older metadata cannot prove a complete live projection across a crash.
fn unknown_projection_uncertain() -> bool {
    true
}

#[derive(Debug, Clone)]
struct CachedTurn {
    checkpoint: DurableTurnCheckpoint,
    journal_event_count: usize,
}

impl DurableTurnCheckpoint {
    pub(crate) fn initial_prompt_inputs(
        &self,
    ) -> Result<Vec<orchestrator::QueuedPromptInput>, DurableTurnStoreError> {
        let mut inputs = match (self.skill_prompt.as_ref(), self.prompt_identity.as_ref()) {
            (Some(skill), None) => skill.queued_prompt_inputs(&self.images),
            (None, Some(identity)) => {
                vec![identity.queued_prompt_input(self.prompt.clone(), &self.images)]
            }
            _ => {
                return Err(DurableTurnStoreError::Storage(
                    "durable turn prompt identity is inconsistent".into(),
                ))
            }
        };
        self.correlate_prompt_inputs(&mut inputs);
        Ok(inputs)
    }

    /// Bind only the explicitly identified user row; expansion/meta rows retain
    /// their own identities and coalesced inputs keep their member tokens.
    pub(crate) fn correlate_prompt_inputs(&self, inputs: &mut [orchestrator::QueuedPromptInput]) {
        let row_id = self
            .prompt_identity
            .as_ref()
            .map(|identity| identity.message_id)
            .or_else(|| {
                self.skill_prompt
                    .as_ref()
                    .map(|skill| skill.input_message_ids[0])
            });
        let Some(row_id) = row_id else {
            return;
        };
        for input in inputs {
            if input.message_id == Some(row_id) && input.transcript_row_token.is_none() {
                input.transcript_row_token = Some(format!(
                    "client-turn:{}",
                    self.original_client_turn_id.unwrap_or(self.turn_id)
                ));
            }
        }
    }

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
            self.correlate_prompt_inputs(&mut inputs);
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
        let mut input = identity.queued_prompt_input(join_source_prompts(&prompts), &images);
        // A persisted coalesced row may represent several UI prompts. Preserve
        // each client correlator without guessing from duplicate prompt text.
        let turns = members
            .iter()
            .map(|member| {
                member
                    .original_client_turn_id
                    .unwrap_or(member.turn_id)
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(",");
        input.transcript_row_token = Some(format!("client-turn:{turns}"));
        Ok(vec![input])
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
            revision: self.revision,
            state: self
                .state
                .clone()
                .expect("private queued prompts do not have recovery snapshots"),
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            safe_to_resume: self.safe_to_resume,
            projection_uncertain: self.projection_uncertain,
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
        self.replay_gap_reason(after_sequence).is_some()
    }

    pub(crate) fn replay_gap_reason(&self, after_sequence: Option<u64>) -> Option<&'static str> {
        let after = after_sequence.unwrap_or(0);
        // A cursor beyond the recovered tail can result from a crash after a
        // live, unsynced append. It is also a gap; returning an empty replay
        // would falsely tell the client its higher cursor is authoritative.
        if after > self.last_sequence {
            Some("replay_cursor_ahead")
        } else if self.first_sequence != 0 && after.saturating_add(1) < self.first_sequence {
            Some("replay_history_truncated")
        } else {
            None
        }
    }

    fn can_resume_without_user(&self) -> bool {
        self.state.is_some()
            && self.queue_delivery == QueueDeliveryState::Claimed
            && self.safe_to_resume
            && !self.projection_uncertain
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
            cancelled_before_claim: self.cancelled_before_claim,
            coalesced_batch: self.coalesced_batch.clone(),
            revision: self.revision,
            state: self.state.clone(),
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            safe_to_resume: self.safe_to_resume,
            projection_uncertain: self.projection_uncertain,
            reason: self.reason.clone(),
            events: Vec::new(),
        }
    }

    fn from_parts(metadata: StoredTurnMetadata, journal_events: Vec<DurableTurnEvent>) -> Self {
        // Older metadata files embedded their retained events. If a process
        // appends to one of those files, the new journal contains only the
        // post-migration suffix, so recovery must merge both sources instead
        // of silently discarding the legacy prefix.
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
            cancelled_before_claim: metadata.cancelled_before_claim,
            coalesced_batch: metadata.coalesced_batch,
            revision: metadata.revision,
            state: metadata.state,
            first_sequence: metadata.first_sequence,
            last_sequence: metadata.last_sequence,
            safe_to_resume: metadata.safe_to_resume && !metadata.projection_uncertain,
            projection_uncertain: metadata.projection_uncertain,
            reason: metadata.reason,
            events,
        };
        checkpoint.reconcile_sequences();

        // The journal is the commit record for streamed events. A process can
        // die after appending a terminal event but before the follow-up
        // metadata transition, leaving a stale `Running` metadata file. On a
        // cold load, the last retained terminal event is therefore
        // authoritative only until a terminal metadata transition commits.
        // Once terminal, metadata owns the state and reason; event stop reasons
        // do not necessarily match the host's persisted recovery reason.
        if checkpoint
            .state
            .as_ref()
            .is_some_and(|state| !state.is_terminal())
        {
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
    message: Option<ClientTextDto>,
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
            "max_turns" => Some((TurnRecoveryStateDto::Failed, Some("max_turns".to_string()))),
            "cancelled" => Some((
                TurnRecoveryStateDto::Cancelled,
                Some("cancelled".to_string()),
            )),
            _ => None,
        },
        "error" => Some((
            TurnRecoveryStateDto::Failed,
            event.message.map(|message| message.display()),
        )),
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
    #[error("session {session_id} already has a live turn owner (turn {turn_id:?})")]
    LiveOwner {
        session_id: String,
        turn_id: Option<u64>,
    },
    #[error("turn {turn_id} in session {session_id} has a pending admission")]
    PendingAdmission { session_id: String, turn_id: u64 },
    #[error("queued batch member {turn_id} in session {session_id} has already been claimed")]
    BatchAlreadyClaimed { session_id: String, turn_id: u64 },
    #[error("durable turn storage failure: {0}")]
    Storage(String),
}

#[derive(Default)]
struct LiveTurnOwners {
    next_generation: u64,
    sessions: HashMap<String, (u64, Option<u64>)>,
    work: HashMap<(String, Option<u64>), u64>,
}

pub(crate) struct PendingAdmissionLease {
    store: Arc<DurableTurnStore>,
    session_id: String,
    turn_id: u64,
}

struct PendingAdmissionEntry {
    submission: Weak<super::host::PendingSubmission>,
    unresolved: bool,
}

impl PendingAdmissionLease {
    /// A failed cancellation write may have reached disk. Retain the
    /// in-process admission fence until a recovery protocol can resolve it.
    pub(crate) fn mark_unresolved(&self) {
        let mut admissions = self
            .store
            .pending_admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = admissions.get_mut(&(self.session_id.clone(), self.turn_id)) {
            entry.unresolved = true;
        }
    }
}

impl Drop for PendingAdmissionLease {
    fn drop(&mut self) {
        let mut admissions = self
            .store
            .pending_admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = (self.session_id.clone(), self.turn_id);
        if admissions.get(&key).is_some_and(|entry| entry.unresolved) {
            if let Some(entry) = admissions.get_mut(&key) {
                entry.submission = Weak::new();
            }
        } else {
            admissions.remove(&key);
        }
    }
}

/// An execution reservation. Blocking work retains a clone until it really
/// exits, keeping the session slot occupied even after its engine is dropped.
#[derive(Clone)]
pub(crate) struct LiveTurnOwnerLease {
    inner: Arc<LiveTurnOwnerLeaseInner>,
}

struct LiveTurnOwnerLeaseInner {
    work: Arc<LiveTurnWorkLeaseInner>,
}

/// A detached background hook retains only its originating turn identity,
/// allowing a later turn while preventing recovery of this still-live one.
#[derive(Clone)]
pub(crate) struct LiveTurnWorkLease {
    inner: Arc<LiveTurnWorkLeaseInner>,
}

struct LiveTurnWorkLeaseInner {
    store: Arc<DurableTurnStore>,
    session_id: String,
    turn_id: Option<u64>,
    generation: u64,
}

impl LiveTurnOwnerLease {
    /// Use only for work which is explicitly allowed to outlive its turn and
    /// overlap the next turn, such as an asynchronous PostToolBatch hook.
    pub(crate) fn detached_work_lease(&self) -> LiveTurnWorkLease {
        LiveTurnWorkLease {
            inner: self.inner.work.clone(),
        }
    }
}

impl Drop for LiveTurnOwnerLeaseInner {
    fn drop(&mut self) {
        let work = &self.work;
        let mut owners = self
            .work
            .store
            .live_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.sessions.get(&work.session_id).map(|entry| entry.0) == Some(work.generation) {
            owners.sessions.remove(&work.session_id);
        }
        drop(owners);
    }
}

impl Drop for LiveTurnWorkLeaseInner {
    fn drop(&mut self) {
        let mut owners = self
            .store
            .live_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (self.session_id.clone(), self.turn_id);
        if owners.work.get(&key) == Some(&self.generation) {
            owners.work.remove(&key);
        }
    }
}

/// All handles in this process for one root share a store, including its cache
/// and session index. This is not a cross-process lock: callers in another
/// process must not access the same root concurrently.
pub(crate) struct DurableTurnStore {
    root: PathBuf,
    delivery_gates: super::turn_delivery::TurnDeliveryGates,
    // Keep only a weak reference: the submitting command owns the admission,
    // while every handle for this store can find the same cancellation target.
    pending_admissions: Mutex<HashMap<(String, u64), PendingAdmissionEntry>>,
    /// Lock order: live owners, then cache, then session index.
    live_owners: Mutex<LiveTurnOwners>,
    lock: Mutex<HashMap<PathBuf, CachedTurn>>,
    /// The first scan indexes checkpoint filenames for a session. Store writes
    /// keep an existing index current so later admissions do not rescan disk.
    session_turn_ids: Mutex<HashMap<String, Vec<u64>>>,
    #[cfg(test)]
    journal_sync_count: AtomicUsize,
    #[cfg(test)]
    fail_after_metadata_publish: AtomicBool,
    #[cfg(test)]
    fail_before_metadata_write: AtomicBool,
    #[cfg(test)]
    midturn_ack_barrier: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    #[cfg(test)]
    fail_before_metadata_sync: AtomicBool,
    #[cfg(test)]
    fail_after_journal_flush: AtomicBool,
    #[cfg(test)]
    fail_before_journal_write: AtomicBool,
}

impl DurableTurnStore {
    /// Return the process-wide store for this directory, even when different
    /// path spellings name the same existing ancestor. Do not create the store
    /// directory here: constructing an engine is not itself a durable turn.
    pub(crate) fn open_shared(root: PathBuf) -> Result<Arc<Self>, DurableTurnStoreError> {
        static STORES: OnceLock<Mutex<HashMap<PathBuf, Weak<DurableTurnStore>>>> = OnceLock::new();
        let root = canonical_store_root(&root)?;
        let mut stores = STORES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(store) = stores.get(&root).and_then(Weak::upgrade) {
            return Ok(store);
        }
        stores.retain(|_, store| store.strong_count() != 0);
        let store = Arc::new(Self::new_unshared(root.clone()));
        stores.insert(root, Arc::downgrade(&store));
        Ok(store)
    }

    /// Construct an independent store only for cold-recovery test fixtures.
    #[cfg(test)]
    pub(crate) fn new(root: PathBuf) -> Self {
        Self::new_unshared(root)
    }

    // Production engine construction must go through `open_shared`.
    fn new_unshared(root: PathBuf) -> Self {
        Self {
            root,
            delivery_gates: super::turn_delivery::TurnDeliveryGates::default(),
            pending_admissions: Mutex::new(HashMap::new()),
            live_owners: Mutex::new(LiveTurnOwners::default()),
            lock: Mutex::new(HashMap::new()),
            session_turn_ids: Mutex::new(HashMap::new()),
            #[cfg(test)]
            journal_sync_count: AtomicUsize::new(0),
            #[cfg(test)]
            fail_after_metadata_publish: AtomicBool::new(false),
            #[cfg(test)]
            fail_before_metadata_write: AtomicBool::new(false),
            #[cfg(test)]
            midturn_ack_barrier: Mutex::new(None),
            #[cfg(test)]
            fail_before_metadata_sync: AtomicBool::new(false),
            #[cfg(test)]
            fail_after_journal_flush: AtomicBool::new(false),
            #[cfg(test)]
            fail_before_journal_write: AtomicBool::new(false),
        }
    }

    pub(crate) fn register_pending_admission(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: u64,
        make_submission: impl FnOnce(PendingAdmissionLease) -> Arc<super::host::PendingSubmission>,
    ) -> Result<Arc<super::host::PendingSubmission>, DurableTurnStoreError> {
        let mut admissions = self
            .pending_admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = (session_id.to_string(), turn_id);
        if admissions.contains_key(&key) {
            return Err(DurableTurnStoreError::PendingAdmission {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        let pending = make_submission(PendingAdmissionLease {
            store: self.clone(),
            session_id: session_id.to_string(),
            turn_id,
        });
        admissions.insert(key, PendingAdmissionEntry {
            submission: Arc::downgrade(&pending),
            unresolved: false,
        });
        Ok(pending)
    }

    pub(crate) fn pending_admission(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Option<Arc<super::host::PendingSubmission>> {
        self.pending_admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(session_id.to_string(), turn_id))
            .and_then(|entry| entry.submission.upgrade())
    }

    pub(crate) fn with_no_pending_admission<T>(
        &self,
        session_id: &str,
        turn_id: u64,
        operation: impl FnOnce() -> Result<T, DurableTurnStoreError>,
    ) -> Result<T, DurableTurnStoreError> {
        self.with_no_pending_admissions(session_id, &[turn_id], operation)
    }

    pub(crate) fn with_no_pending_admissions<T>(
        &self,
        session_id: &str,
        turn_ids: &[u64],
        operation: impl FnOnce() -> Result<T, DurableTurnStoreError>,
    ) -> Result<T, DurableTurnStoreError> {
        let admissions = self
            .pending_admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(turn_id) = turn_ids
            .iter()
            .copied()
            .find(|id| admissions.contains_key(&(session_id.to_string(), *id)))
        {
            return Err(DurableTurnStoreError::PendingAdmission {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        operation()
    }

    /// Cancel one still-private queue entry while its exact admission remains
    /// registered. This does not borrow authority from a live turn owner.
    /// Queued dispatchers hold the admission lock before taking the store lock.
    pub(crate) fn cancel_unclaimed_admission(
        &self,
        session_id: &str,
        turn_id: u64,
        lease: &PendingAdmissionLease,
        pending: &Arc<super::host::PendingSubmission>,
    ) -> Result<Option<TurnRecoverySnapshotDto>, DurableTurnStoreError> {
        let reject = || DurableTurnStoreError::PendingAdmission {
            session_id: session_id.to_string(),
            turn_id,
        };
        if !std::ptr::eq(self, Arc::as_ptr(&lease.store))
            || lease.session_id != session_id
            || lease.turn_id != turn_id
        {
            return Err(reject());
        }
        let admissions = self
            .pending_admissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(entry) = admissions.get(&(session_id.to_string(), turn_id)) else {
            return Err(reject());
        };
        if entry.unresolved
            || !entry
                .submission
                .upgrade()
                .is_some_and(|registered| Arc::ptr_eq(&registered, pending))
        {
            return Err(reject());
        }

        let mut cache = self.lock.lock().unwrap_or_else(|e| e.into_inner());
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
        if target.state.is_some()
            || target.queue_delivery != QueueDeliveryState::Pending
            || target.coalesced_batch.is_some()
            || checkpoints.iter().any(|checkpoint| {
                checkpoint
                    .coalesced_batch
                    .as_ref()
                    .is_some_and(|batch| batch.member_turn_ids.contains(&turn_id))
            })
        {
            return Ok(None);
        }
        self.update_if_locked(&mut cache, session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_some()
                || checkpoint.queue_delivery != QueueDeliveryState::Pending
                || checkpoint.coalesced_batch.is_some()
            {
                return Ok(None);
            }
            checkpoint.queue_delivery = QueueDeliveryState::Cancelled;
            checkpoint.cancelled_before_claim = true;
            checkpoint.state = Some(TurnRecoveryStateDto::Cancelled);
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("explicit_cancel".into());
            Ok(Some(checkpoint.snapshot()))
        })
    }

    /// Return the shared delivery gate for one durable turn. The caller must
    /// await its slot outside owner/store locks and revalidate ownership after
    /// acquisition.
    pub(crate) fn delivery_gate(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: u64,
    ) -> Result<Arc<super::turn_delivery::TurnDeliveryGate>, DurableTurnStoreError> {
        relative_path(session_id, turn_id)?;
        Ok(self.delivery_gates.get(self.clone(), session_id, turn_id))
    }

    /// Reserve this session before admitting an executor. Clones of the
    /// returned lease must be transferred to any work that can outlive it.
    pub(crate) fn reserve_live_owner(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: Option<u64>,
    ) -> Result<LiveTurnOwnerLease, DurableTurnStoreError> {
        relative_path(session_id, 0)?;
        let mut owners = self
            .live_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, active_turn_id)) = owners.sessions.get(session_id) {
            return Err(DurableTurnStoreError::LiveOwner {
                session_id: session_id.to_string(),
                turn_id: *active_turn_id,
            });
        }
        let work_key = (session_id.to_string(), turn_id);
        if owners.work.contains_key(&work_key) {
            return Err(DurableTurnStoreError::LiveOwner {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        let generation = owners.next_generation.checked_add(1).ok_or_else(|| {
            DurableTurnStoreError::Storage("mobile live owner generation exhausted".into())
        })?;
        owners.next_generation = generation;
        owners
            .sessions
            .insert(session_id.to_string(), (generation, turn_id));
        owners.work.insert(work_key, generation);
        drop(owners);
        Ok(LiveTurnOwnerLease {
            inner: Arc::new(LiveTurnOwnerLeaseInner {
                work: Arc::new(LiveTurnWorkLeaseInner {
                    store: self.clone(),
                    session_id: session_id.to_string(),
                    turn_id,
                    generation,
                }),
            }),
        })
    }

    /// Run an inactive-owner decision and its store mutations atomically with
    /// respect to new reservations. The closure must be synchronous and must
    /// not call listeners, await, or acquire the live-owner lock recursively.
    pub(crate) fn with_no_live_owner<T>(
        &self,
        session_id: &str,
        turn_id: Option<u64>,
        operation: impl FnOnce() -> Result<T, DurableTurnStoreError>,
    ) -> Result<T, DurableTurnStoreError> {
        relative_path(session_id, 0)?;
        let owners = self
            .live_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, turn_id)) = owners.sessions.get(session_id) {
            return Err(DurableTurnStoreError::LiveOwner {
                session_id: session_id.to_string(),
                turn_id: *turn_id,
            });
        }
        if owners.work.contains_key(&(session_id.to_string(), turn_id)) {
            return Err(DurableTurnStoreError::LiveOwner {
                session_id: session_id.to_string(),
                turn_id,
            });
        }
        let result = operation();
        drop(owners);
        result
    }

    /// Perform an owning executor's synchronous mutation while its exact
    /// reservation is still current. No await, callbacks, or nested owner
    /// operations are permitted inside the closure.
    pub(crate) fn with_live_owner<T>(
        &self,
        session_id: &str,
        lease: &LiveTurnOwnerLease,
        target_turn_id: Option<u64>,
        operation: impl FnOnce() -> Result<T, DurableTurnStoreError>,
    ) -> Result<T, DurableTurnStoreError> {
        let work = &lease.inner.work;
        if !std::ptr::eq(self, Arc::as_ptr(&work.store)) || work.session_id != session_id {
            return Err(DurableTurnStoreError::Storage(
                "mobile live owner does not match this session".into(),
            ));
        }
        let owners = self
            .live_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.sessions.get(session_id).map(|entry| entry.0) != Some(work.generation) {
            return Err(DurableTurnStoreError::Storage(
                "mobile live owner is no longer current".into(),
            ));
        }
        if owners
            .work
            .get(&(session_id.to_string(), target_turn_id))
            .is_some_and(|generation| *generation != work.generation)
        {
            return Err(DurableTurnStoreError::LiveOwner {
                session_id: session_id.to_string(),
                turn_id: target_turn_id,
            });
        }
        let result = operation();
        drop(owners);
        result
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
            cancelled_before_claim: false,
            coalesced_batch: None,
            revision: 1,
            state: Some(TurnRecoveryStateDto::Running),
            first_sequence: 0,
            last_sequence: 0,
            safe_to_resume: true,
            projection_uncertain: false,
            reason: None,
            events: Vec::new(),
        };
        if let Err(error) = self.write_metadata_relative(&relative, &checkpoint) {
            cache.remove(&relative);
            self.forget_session_index(session_id);
            return Err(error);
        }
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
            cancelled_before_claim: false,
            coalesced_batch: None,
            revision: 1,
            state: None,
            first_sequence: 0,
            last_sequence: 0,
            safe_to_resume: false,
            projection_uncertain: false,
            reason: None,
            events: Vec::new(),
        };
        if let Err(error) = self.write_metadata_relative(&relative, &checkpoint) {
            cache.remove(&relative);
            self.forget_session_index(session_id);
            return Err(error);
        }
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
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
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
        if members.iter().any(|member| {
            member.state.is_some()
                || !matches!(
                    member.queue_delivery,
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                )
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
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
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
            checkpoint.safe_to_resume = false;
            checkpoint.projection_uncertain = true;
            checkpoint.reason = Some("mid_turn_claim_requires_confirmation".into());
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
        let mut cache = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        if !checkpoints
            .iter()
            .any(|checkpoint| checkpoint.turn_id == turn_id)
        {
            return Err(DurableTurnStoreError::NotFound {
                session_id: session_id.to_owned(),
                turn_id,
            });
        }
        if checkpoints.iter().any(|checkpoint| {
            checkpoint.turn_id != turn_id
                && checkpoint
                    .coalesced_batch
                    .as_ref()
                    .is_some_and(|batch| batch.member_turn_ids.contains(&turn_id))
        }) {
            return Err(DurableTurnStoreError::Terminal {
                session_id: session_id.to_owned(),
                turn_id,
            });
        }
        let claimed = self.update_if_locked(&mut cache, session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_some()
                || !matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                )
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = Some(TurnRecoveryStateDto::Running);
            checkpoint.queue_delivery = QueueDeliveryState::Claimed;
            checkpoint.safe_to_resume = !checkpoint.projection_uncertain;
            checkpoint.reason = None;
            Ok(Some(checkpoint.clone()))
        })?;
        Ok(claimed.expect("claim always updates the checkpoint"))
    }

    /// Recreate a checkpoint produced by an older process for recovery tests.
    #[cfg(test)]
    pub(crate) fn claim_legacy_batch_for_test(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(), DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.queue_delivery != QueueDeliveryState::CoalescedClaimed {
                return Err(DurableTurnStoreError::NotClaimed {
                    session_id: session_id.into(),
                    turn_id,
                });
            }
            checkpoint.state = Some(TurnRecoveryStateDto::Running);
            checkpoint.queue_delivery = QueueDeliveryState::Claimed;
            Ok(())
        })
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
            checkpoint.safe_to_resume = !checkpoint.projection_uncertain;
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
            checkpoint.safe_to_resume = !checkpoint.projection_uncertain;
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

    fn forget_session_index(&self, session_id: &str) {
        self.session_turn_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
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

            let sequence = cached
                .checkpoint
                .last_sequence
                .checked_add(1)
                .ok_or_else(|| {
                    DurableTurnStoreError::Storage(
                        "mobile event sequence space is exhausted".into(),
                    )
                })?;
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
            checkpoint.safe_to_resume =
                safe_to_resume && !state.is_terminal() && !checkpoint.projection_uncertain;
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
            checkpoint.safe_to_resume =
                safe_to_resume && !state.is_terminal() && !checkpoint.projection_uncertain;
            checkpoint.reason = reason;
            if state.is_terminal() && state != TurnRecoveryStateDto::Cancelled {
                checkpoint.queue_delivery = QueueDeliveryState::Delivered;
            }
            Ok(Some(checkpoint.snapshot()))
        })
    }

    /// Persist the tool side-effect boundary before the orchestrator may
    /// schedule a tool or its hooks. Repeated admissions are idempotent.
    pub(crate) fn mark_tool_dispatch_unsafe(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(), DurableTurnStoreError> {
        self.update_if(session_id, turn_id, |checkpoint| {
            if !matches!(
                checkpoint.state.as_ref(),
                Some(TurnRecoveryStateDto::Running | TurnRecoveryStateDto::WaitingForUser)
            ) {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_owned(),
                    turn_id,
                });
            }
            if !checkpoint.safe_to_resume {
                return Ok(None);
            }
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("tool_boundary_requires_confirmation".into());
            Ok(Some(()))
        })?;
        Ok(())
    }

    /// Force the unsafe recovery boundary before exposing the first material
    /// turn output. Even an acknowledged journal append can be lost on a
    /// crash before its next fsync, so the decision must precede both retained
    /// and live-only delivery. Rewrite the metadata even when already unsafe:
    /// this call must not acknowledge an earlier ambiguous metadata write.
    pub(crate) fn fence_observed_turn_output(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(), DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            let Some(state) = checkpoint.state.as_ref() else {
                return Err(DurableTurnStoreError::NotClaimed {
                    session_id: session_id.to_owned(),
                    turn_id,
                });
            };
            if checkpoint.safe_to_resume && !state.is_terminal() {
                checkpoint.reason = Some("turn_output_requires_confirmation".into());
            }
            checkpoint.safe_to_resume = false;
            checkpoint.projection_uncertain = true;
            Ok(())
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
                    && ((checkpoint.state.is_none()
                        && matches!(
                            checkpoint.queue_delivery,
                            QueueDeliveryState::CoalescedClaimed
                        ))
                        || (turn_id != batch.owner_turn_id
                            && (checkpoint.state.is_some()
                                || matches!(
                                    checkpoint.queue_delivery,
                                    QueueDeliveryState::Claimed | QueueDeliveryState::Delivered
                                ))))
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
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                );
            if checkpoint.queue_delivery == QueueDeliveryState::SlashClaimed {
                // A claimed slash may already have produced effects. Publish a
                // correlated terminal snapshot, but retain projection uncertainty.
                checkpoint.projection_uncertain = true;
            }
            if checkpoint.state.is_none()
                && matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                )
            {
                checkpoint.cancelled_before_claim = true;
            }
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
            })
        });
        if owner.is_some_and(|checkpoint| {
            checkpoint.state.is_some()
                || !matches!(
                    checkpoint.queue_delivery,
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                )
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
                checkpoint.cancelled_before_claim = true;
                checkpoint.safe_to_resume = false;
                checkpoint.reason = Some("explicit_cancel".to_string());
                Ok(Some(()))
            })?;
        }
        Ok(Some(member_ids))
    }

    /// Publish an explicitly cancelled private entry as a client-visible
    /// terminal turn. Its queue cancellation must already be durable.
    pub(crate) fn expose_cancelled_pending(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<TurnRecoverySnapshotDto, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_some()
                || checkpoint.queue_delivery != QueueDeliveryState::Cancelled
            {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = Some(TurnRecoveryStateDto::Cancelled);
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("explicit_cancel".into());
            Ok(checkpoint.snapshot())
        })
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

    /// Describe exactly one queued member from a single locked store view.
    /// The batch owner is its first queued member, not the active model turn.
    pub(crate) fn queued_turn_status(
        &self,
        session_id: &str,
        member_turn_id: u64,
    ) -> Result<QueuedTurnStatusDto, DurableTurnStoreError> {
        let mut cache = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        let target = checkpoints
            .iter()
            .find(|item| item.turn_id == member_turn_id)
            .ok_or_else(|| DurableTurnStoreError::NotFound {
                session_id: session_id.to_owned(),
                turn_id: member_turn_id,
            })?;
        if target.cancelled_before_claim {
            if target.queue_delivery != QueueDeliveryState::Cancelled
                || target
                    .state
                    .as_ref()
                    .is_some_and(|state| *state != TurnRecoveryStateDto::Cancelled)
            {
                return Err(DurableTurnStoreError::Storage(
                    "inconsistent pre-claim cancellation marker".into(),
                ));
            }
            return Ok(QueuedTurnStatusDto::Cancelled);
        }
        if let Some(state) = &target.state {
            return Ok(if state.is_terminal() {
                QueuedTurnStatusDto::Terminal {
                    state: state.clone(),
                }
            } else if *state == TurnRecoveryStateDto::Running {
                QueuedTurnStatusDto::Running
            } else {
                QueuedTurnStatusDto::ClaimedUncertain
            });
        }
        let mut owners = checkpoints.iter().filter(|item| {
            item.coalesced_batch
                .as_ref()
                .is_some_and(|batch| batch.member_turn_ids.contains(&member_turn_id))
        });
        if let Some(owner) = owners.next() {
            if owners.next().is_some() {
                return Err(DurableTurnStoreError::Storage(
                    "queued member belongs to multiple batches".into(),
                ));
            }
            let batch = owner.coalesced_batch.as_ref().expect("matched batch");
            let unique = batch
                .member_turn_ids
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            if batch.owner_turn_id != owner.turn_id
                || batch.member_turn_ids.first() != Some(&owner.turn_id)
                || unique.len() != batch.member_turn_ids.len()
                || batch.member_identities.len() != batch.member_turn_ids.len()
                || !batch
                    .member_identities
                    .iter()
                    .zip(&batch.member_turn_ids)
                    .all(|(identity, id)| identity.turn_id == *id)
                || batch
                    .member_identities
                    .last()
                    .is_none_or(|identity| identity.prompt_identity != batch.aggregate_identity)
            {
                return Err(DurableTurnStoreError::Storage(
                    "queued batch identity is inconsistent".into(),
                ));
            }
            if owner.state.is_none() && owner.queue_delivery == QueueDeliveryState::Delivered {
                // A prior write may have returned an ambiguous error after its
                // rename. Confirm the file and directory entries are durable
                // before exposing this batch as a delivered receipt.
                self.sync_metadata_relative(&relative_path(session_id, owner.turn_id)?)?;
                return Ok(QueuedTurnStatusDto::Delivered {
                    batch_owner_turn_id: batch.owner_turn_id,
                    member_turn_ids: batch.member_turn_ids.clone(),
                    transcript_uuid: batch.aggregate_identity.message_id.as_uuid().to_string(),
                });
            }
            if owner.queue_delivery == QueueDeliveryState::Cancelled {
                if owner.cancelled_before_claim {
                    self.sync_metadata_relative(&relative_path(session_id, owner.turn_id)?)?;
                    return Ok(QueuedTurnStatusDto::Cancelled);
                }
                return Ok(QueuedTurnStatusDto::ClaimedUncertain);
            }
            if owner.state.is_some()
                || matches!(
                    owner.queue_delivery,
                    QueueDeliveryState::CoalescedClaimed | QueueDeliveryState::Claimed
                )
            {
                return Ok(QueuedTurnStatusDto::ClaimedUncertain);
            }
        }
        Ok(match target.queue_delivery {
            QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending => {
                QueuedTurnStatusDto::Pending
            }
            _ => QueuedTurnStatusDto::ClaimedUncertain,
        })
    }

    pub(crate) fn resume(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(ResumeDisposition, Option<TurnRecoverySnapshotDto>), DurableTurnStoreError> {
        let mut cache = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let checkpoints = self.read_session_checkpoints(&mut cache, session_id)?;
        if !checkpoints
            .iter()
            .any(|checkpoint| checkpoint.turn_id == turn_id)
        {
            return Err(DurableTurnStoreError::NotFound {
                session_id: session_id.to_owned(),
                turn_id,
            });
        }
        if checkpoints.iter().any(|checkpoint| {
            checkpoint.turn_id != turn_id
                && checkpoint
                    .coalesced_batch
                    .as_ref()
                    .is_some_and(|batch| batch.member_turn_ids.contains(&turn_id))
                && (checkpoint.state.is_some()
                    || !matches!(
                        checkpoint.queue_delivery,
                        QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                    ))
        }) {
            return Ok((ResumeDisposition::WaitingForUser, None));
        }
        self.update_if_locked(&mut cache, session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_none() {
                return match checkpoint.queue_delivery {
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending => {
                        Ok((ResumeDisposition::Queued, None))
                    }
                    QueueDeliveryState::SlashClaimed => Ok((ResumeDisposition::Terminal, None)),
                    QueueDeliveryState::BashClaimed | QueueDeliveryState::CoalescedClaimed => {
                        Ok((ResumeDisposition::WaitingForUser, None))
                    }
                    QueueDeliveryState::Delivered | QueueDeliveryState::Cancelled => {
                        Ok((ResumeDisposition::Terminal, None))
                    }
                    QueueDeliveryState::Claimed => Err(DurableTurnStoreError::NotClaimed {
                        session_id: session_id.to_string(),
                        turn_id,
                    }),
                }
                .map(Some);
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
            Ok(Some((disposition, Some(checkpoint.snapshot()))))
        })
        .map(|value| value.expect("resume always updates the checkpoint"))
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
        if let Err(error) =
            self.write_compacted_journal_relative(&relative, &cached.checkpoint.events)
        {
            cache.remove(&relative);
            return Err(error);
        }
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
        // Mutators may return a snapshot or a checkpoint. Reconcile the
        // retention window before they create that value so it agrees with
        // the metadata committed below, even within the compaction slack.
        checkpoint.reconcile_sequences();
        // Mutators may return a checkpoint or snapshot. Make the candidate
        // revision visible to them so their returned value is the committed
        // revision, not the previous one. A no-op does not consume a version.
        let next_revision = checkpoint.revision.checked_add(1);
        if let Some(revision) = next_revision {
            checkpoint.revision = revision;
        }
        let Some(value) = mutate(&mut checkpoint)? else {
            return Ok(None);
        };
        if next_revision.is_none() {
            return Err(DurableTurnStoreError::Storage(
                "mobile checkpoint revision space is exhausted".into(),
            ));
        }
        checkpoint.reconcile_sequences();
        if let Err(error) = self
            .sync_journal_relative(&relative)
            .and_then(|()| self.write_metadata_relative(&relative, &checkpoint))
        {
            // An atomic rename may have succeeded even when its parent fsync
            // failed. Never reuse a cache that can predate the on-disk state.
            cache.remove(&relative);
            return Err(error);
        }
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
        let journal_relative = journal_relative_path(relative)?;
        let journal = self.root.join(&journal_relative);
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
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        lingxi_core::host::rooted_fs::sync_parent_pinned(&self.root, &journal_relative, None)
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
        // A prior rename can be visible despite a failed parent fsync. Before
        // returning that version, confirm both the final file and its entry.
        self.sync_metadata_relative(relative)?;
        let metadata_state = (
            metadata.state.clone(),
            metadata.safe_to_resume,
            metadata.reason.clone(),
            metadata.queue_delivery,
        );
        let journal_events = self.read_journal_relative(relative)?;
        let journal_event_count = journal_events.len();
        let mut checkpoint = DurableTurnCheckpoint::from_parts(metadata, journal_events);
        let recovered_state = (
            checkpoint.state.clone(),
            checkpoint.safe_to_resume,
            checkpoint.reason.clone(),
            checkpoint.queue_delivery,
        );
        if recovered_state != metadata_state {
            // A synced terminal event can precede its metadata transition. Do
            // not expose the inferred state until its new version is durable.
            checkpoint.revision = checkpoint.revision.checked_add(1).ok_or_else(|| {
                DurableTurnStoreError::Storage(
                    "mobile checkpoint revision space is exhausted".into(),
                )
            })?;
            self.sync_journal_relative(relative)?;
            self.write_metadata_relative(relative, &checkpoint)?;
        }
        Ok(CachedTurn {
            checkpoint,
            journal_event_count,
        })
    }

    fn sync_store_directory_entries(&self, relative: &Path) -> Result<(), DurableTurnStoreError> {
        let root_parent = self.root.parent().ok_or_else(|| {
            DurableTurnStoreError::Storage("durable turn root has no parent".into())
        })?;
        let root_name = self.root.file_name().ok_or_else(|| {
            DurableTurnStoreError::Storage("durable turn root has no name".into())
        })?;
        // Durability above this root depends on the host establishing its home
        // directory durably. Sync both directory entries owned by this store
        // before acknowledging a checkpoint, including after an ambiguous
        // failure.
        lingxi_core::host::rooted_fs::sync_parent_pinned(root_parent, Path::new(root_name), None)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        let session = relative.parent().ok_or_else(|| {
            DurableTurnStoreError::Storage("durable turn path has no session directory".into())
        })?;
        lingxi_core::host::rooted_fs::sync_parent_pinned(&self.root, session, None)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))
    }

    fn sync_metadata_relative(&self, relative: &Path) -> Result<(), DurableTurnStoreError> {
        #[cfg(test)]
        if self
            .fail_before_metadata_sync
            .swap(false, Ordering::Relaxed)
        {
            return Err(DurableTurnStoreError::Storage(
                "injected failure before metadata sync".into(),
            ));
        }
        let file = lingxi_core::host::rooted_fs::open_read_file_pinned(&self.root, relative, None)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        file.sync_all()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        lingxi_core::host::rooted_fs::sync_parent_pinned(&self.root, relative, None)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        self.sync_store_directory_entries(relative)
    }

    fn write_metadata_relative(
        &self,
        relative: &Path,
        checkpoint: &DurableTurnCheckpoint,
    ) -> Result<(), DurableTurnStoreError> {
        #[cfg(test)]
        if self
            .fail_before_metadata_write
            .swap(false, Ordering::Relaxed)
        {
            return Err(DurableTurnStoreError::Storage(
                "injected failure before metadata write".into(),
            ));
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        if let Some(parent) = relative.parent() {
            std::fs::create_dir_all(self.root.join(parent))
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        }
        self.sync_store_directory_entries(relative)?;
        let mut body = serde_json::to_vec_pretty(&checkpoint.stored_metadata())
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        body.push(b'\n');
        lingxi_core::host::rooted_fs::atomic_write(
            &self.root,
            relative,
            &body,
            lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
        )
        .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        #[cfg(test)]
        if self
            .fail_after_metadata_publish
            .swap(false, Ordering::Relaxed)
        {
            return Err(DurableTurnStoreError::Storage(
                "injected failure after metadata publication".into(),
            ));
        }
        Ok(())
    }

    fn append_journal_relative(
        &self,
        relative: &Path,
        event: &DurableTurnEvent,
        durable: bool,
    ) -> Result<(), DurableTurnStoreError> {
        #[cfg(test)]
        if self
            .fail_before_journal_write
            .swap(false, Ordering::Relaxed)
        {
            return Err(DurableTurnStoreError::Storage(
                "injected failure before journal write".into(),
            ));
        }
        let journal_relative = journal_relative_path(relative)?;
        let journal = self.root.join(&journal_relative);
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
        #[cfg(test)]
        if self.fail_after_journal_flush.swap(false, Ordering::Relaxed) {
            return Err(DurableTurnStoreError::Storage(
                "injected failure after journal flush".into(),
            ));
        }
        if durable {
            file.sync_data()
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
            // sync_data alone does not persist a newly created journal name.
            lingxi_core::host::rooted_fs::sync_parent_pinned(&self.root, &journal_relative, None)
                .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_after_next_journal_flush(&self) {
        self.fail_after_journal_flush.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn fail_before_next_journal_write(&self) {
        self.fail_before_journal_write
            .store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn fail_before_next_metadata_write(&self) {
        self.fail_before_metadata_write.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn fail_after_next_metadata_publish(&self) {
        self.fail_after_metadata_publish
            .store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn set_midturn_ack_barrier(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self
            .midturn_ack_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((entered, release));
    }

    #[cfg(test)]
    pub(crate) fn take_midturn_ack_barrier(
        &self,
    ) -> Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)> {
        self.midturn_ack_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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

/// Resolve an existing root (including symlinks), or its nearest existing
/// ancestor when no turn directory exists yet. Preserve path components as
/// `OsString` rather than merging distinct non-UTF-8 paths via a lossy key.
fn canonical_store_root(root: &Path) -> Result<PathBuf, DurableTurnStoreError> {
    let mut ancestor = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| DurableTurnStoreError::Storage(error.to_string()))?
            .join(root)
    };
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                for component in suffix.into_iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // An existing but dangling symlink is not a missing directory:
                // silently treating it as one could create two registry keys.
                match std::fs::symlink_metadata(&ancestor) {
                    Ok(_) => return Err(DurableTurnStoreError::Storage(error.to_string())),
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(metadata_error) => {
                        return Err(DurableTurnStoreError::Storage(metadata_error.to_string()));
                    }
                }
                let name = ancestor.file_name().ok_or_else(|| {
                    DurableTurnStoreError::Storage(format!(
                        "cannot resolve durable turn root {}: {error}",
                        root.display()
                    ))
                })?;
                suffix.push(name.to_os_string());
                if !ancestor.pop() {
                    return Err(DurableTurnStoreError::Storage(format!(
                        "cannot resolve durable turn root {}: {error}",
                        root.display()
                    )));
                }
            }
            Err(error) => return Err(DurableTurnStoreError::Storage(error.to_string())),
        }
    }
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
    fn metadata_without_projection_marker_is_conservatively_uncertain() {
        let (temp, store) = store();
        store
            .begin("session", 75, "prompt".into(), None, vec![])
            .unwrap();
        let path = temp
            .path()
            .join("turns")
            .join(relative_path("session", 75).unwrap());
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(metadata["projection_uncertain"], false);
        metadata
            .as_object_mut()
            .unwrap()
            .remove("projection_uncertain");
        let legacy = serde_json::to_vec(&metadata).unwrap();
        std::fs::write(&path, &legacy).unwrap();

        let reopened = DurableTurnStore::new(temp.path().join("turns"));
        assert!(
            reopened
                .load("session", 75)
                .unwrap()
                .snapshot()
                .projection_uncertain
        );
        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated["prompt"], metadata["prompt"]);
        assert_eq!(migrated["session_id"], metadata["session_id"]);
        assert_eq!(migrated["projection_uncertain"], true);
        assert_eq!(migrated["safe_to_resume"], false);
        let paused = reopened
            .transition(
                "session",
                75,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("background".into()),
            )
            .unwrap();
        assert!(paused.projection_uncertain);
        assert!(!paused.safe_to_resume);
        assert_eq!(
            reopened.resume("session", 75).unwrap().0,
            ResumeDisposition::WaitingForUser
        );
        assert!(
            DurableTurnStore::new(temp.path().join("turns"))
                .load("session", 75)
                .unwrap()
                .projection_uncertain
        );
    }

    #[test]
    fn claiming_old_queued_metadata_cannot_restore_safe_recovery() {
        let (temp, store) = store();
        let queued = store
            .admit_queued_prompt("session", None, Some(77), "prompt".into(), None, vec![])
            .unwrap();
        let path = temp
            .path()
            .join("turns")
            .join(relative_path("session", queued.turn_id).unwrap());
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        metadata
            .as_object_mut()
            .unwrap()
            .remove("projection_uncertain");
        std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let reopened = DurableTurnStore::new(temp.path().join("turns"));
        let claimed = reopened.claim_pending("session", queued.turn_id).unwrap();
        assert!(claimed.snapshot().projection_uncertain);
        assert!(!claimed.snapshot().safe_to_resume);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        assert_eq!(
            cold.resume("session", queued.turn_id).unwrap().0,
            ResumeDisposition::WaitingForUser
        );

        let fresh = reopened
            .admit_queued_prompt("session", None, Some(78), "fresh".into(), None, vec![])
            .unwrap();
        let fresh_claimed = reopened.claim_pending("session", fresh.turn_id).unwrap();
        assert!(fresh_claimed.safe_to_resume);
        assert!(!fresh_claimed.projection_uncertain);
    }

    #[test]
    fn corrupt_metadata_recovery_fails_without_overwriting_the_source() {
        let (temp, store) = store();
        store
            .begin("session", 76, "prompt".into(), None, vec![])
            .unwrap();
        let path = temp
            .path()
            .join("turns")
            .join(relative_path("session", 76).unwrap());
        let damaged = b"{\"session_id\":\"session\",\"projection_uncertain\":";
        std::fs::write(&path, damaged).unwrap();
        let reopened = DurableTurnStore::new(temp.path().join("turns"));
        assert!(matches!(
            reopened.load("session", 76),
            Err(DurableTurnStoreError::Storage(_))
        ));
        assert!(matches!(
            reopened.resume("session", 76),
            Err(DurableTurnStoreError::Storage(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
    }

    #[test]
    fn unknown_metadata_fields_are_preserved_by_refusing_migration() {
        let (temp, store) = store();
        store
            .begin("session", 79, "prompt".into(), None, vec![])
            .unwrap();
        let path = temp
            .path()
            .join("turns")
            .join(relative_path("session", 79).unwrap());
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        metadata
            .as_object_mut()
            .unwrap()
            .remove("projection_uncertain");
        metadata["future_schema_field"] = serde_json::json!({"data": "retain me"});
        let original = serde_json::to_vec(&metadata).unwrap();
        std::fs::write(&path, &original).unwrap();
        let reopened = DurableTurnStore::new(temp.path().join("turns"));
        assert!(matches!(
            reopened.load("session", 79),
            Err(DurableTurnStoreError::Storage(_))
        ));
        assert!(matches!(
            reopened.resume("session", 79),
            Err(DurableTurnStoreError::Storage(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn shared_factory_reuses_cache_and_session_index_for_a_missing_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("turns");
        let first = DurableTurnStore::open_shared(root.clone()).unwrap();
        assert!(!root.exists(), "opening a store must not create turn data");
        assert!(first.scan_pending("session").unwrap().is_empty());
        let second = DurableTurnStore::open_shared(root).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let admitted = second
            .admit_queued_prompt("session", None, None, "first".into(), None, vec![])
            .unwrap();
        assert_eq!(admitted.turn_id, 1);
        assert_eq!(first.scan_pending("session").unwrap().len(), 1);
        let next = first
            .admit_queued_prompt("session", None, None, "second".into(), None, vec![])
            .unwrap();
        assert_eq!((next.turn_id, next.queue_sequence), (2, 2));
    }

    #[test]
    fn concurrent_first_open_publishes_only_one_shared_store() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("turns");
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let workers = (0..4)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    DurableTurnStore::open_shared(root).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let stores = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(stores.iter().all(|store| Arc::ptr_eq(&stores[0], store)));
    }

    #[cfg(unix)]
    #[test]
    fn shared_factory_normalizes_existing_symlink_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
        let first = DurableTurnStore::open_shared(temp.path().join("turns")).unwrap();
        let second = DurableTurnStore::open_shared(alias.join("turns")).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        first
            .begin("session", 1, "prompt".into(), None, vec![])
            .unwrap();
        let after_creation = DurableTurnStore::open_shared(alias.join("turns")).unwrap();
        assert!(Arc::ptr_eq(&first, &after_creation));
    }

    #[test]
    fn shared_factory_serializes_concurrent_events_and_keeps_state_revision() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("turns");
        let first = DurableTurnStore::open_shared(root.clone()).unwrap();
        first
            .begin("session", 1, "prompt".into(), None, vec![])
            .unwrap();
        let mut workers = Vec::new();
        for worker in 0..4 {
            let root = root.clone();
            workers.push(std::thread::spawn(move || {
                let store = DurableTurnStore::open_shared(root).unwrap();
                for event in 0..25 {
                    store
                        .append_event(
                            "session",
                            1,
                            format!(r#"{{"type":"text_delta","text":"{worker}-{event}"}}"#),
                        )
                        .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let checkpoint = first.load("session", 1).unwrap();
        assert_eq!(checkpoint.revision, 1);
        assert_eq!(checkpoint.last_sequence, 100);
        assert_eq!(
            checkpoint
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=100).collect::<Vec<_>>()
        );
        let snapshot = first
            .transition(
                "session",
                1,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                None,
            )
            .unwrap();
        assert_eq!(snapshot.revision, 2);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let recovered = cold.load("session", 1).unwrap();
        assert_eq!((recovered.revision, recovered.last_sequence), (2, 100));
        assert_eq!(recovered.events, checkpoint.events);
    }

    #[test]
    fn live_owner_lease_outlives_the_handle_and_rejects_other_owners() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("turns");
        let store = DurableTurnStore::open_shared(root.clone()).unwrap();
        store
            .begin("session", 1, "prompt".into(), None, vec![])
            .unwrap();
        let lease = store.reserve_live_owner("session", Some(1)).unwrap();
        let work = lease.clone();
        drop(lease);
        let second = DurableTurnStore::open_shared(root).unwrap();
        assert!(matches!(
            second.reserve_live_owner("session", Some(2)),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        assert!(matches!(
            second.with_no_live_owner("session", Some(1), || second.cancel("session", 1)),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        assert_eq!(
            second.load("session", 1).unwrap().state,
            Some(TurnRecoveryStateDto::Running)
        );
        second
            .with_live_owner("session", &work, Some(1), || Ok(()))
            .unwrap();
        drop(store);
        drop(second);
        let reopened = DurableTurnStore::open_shared(temp.path().join("turns")).unwrap();
        assert!(matches!(
            reopened.reserve_live_owner("session", Some(2)),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        drop(work);
        let new_owner = reopened.reserve_live_owner("session", Some(2)).unwrap();
        reopened
            .with_live_owner("session", &new_owner, Some(2), || Ok(()))
            .unwrap();
    }

    #[test]
    fn detached_work_guards_only_its_old_turn_after_execution_slot_is_released() {
        let temp = tempfile::tempdir().unwrap();
        let store = DurableTurnStore::open_shared(temp.path().join("turns")).unwrap();
        let owner = store.reserve_live_owner("session", Some(1)).unwrap();
        let detached = owner.detached_work_lease();
        drop(owner);
        assert!(matches!(
            store.reserve_live_owner("session", Some(1)),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        assert!(matches!(
            store.with_no_live_owner("session", Some(1), || Ok(())),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        let next = store.reserve_live_owner("session", Some(2)).unwrap();
        assert!(matches!(
            store.with_live_owner("session", &next, Some(1), || Ok(())),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        drop(next);
        assert!(matches!(
            store.reserve_live_owner("session", Some(1)),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        let third = store.reserve_live_owner("session", Some(3)).unwrap();
        drop(detached);
        assert!(matches!(
            store.reserve_live_owner("session", Some(1)),
            Err(DurableTurnStoreError::LiveOwner { .. })
        ));
        store
            .with_live_owner("session", &third, Some(3), || Ok(()))
            .unwrap();
        drop(third);
        assert!(store.reserve_live_owner("session", Some(1)).is_ok());
    }

    #[test]
    fn ambiguous_metadata_publish_invalidates_cache_and_requires_sync_on_reload() {
        let (_temp, store) = store();
        store
            .begin("session", 1, "prompt".into(), None, vec![])
            .unwrap();
        store
            .fail_after_metadata_publish
            .store(true, Ordering::Relaxed);
        assert!(store
            .transition(
                "session",
                1,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                None
            )
            .is_err());
        store
            .fail_before_metadata_sync
            .store(true, Ordering::Relaxed);
        assert!(store.load("session", 1).is_err());
        let recovered = store.load("session", 1).unwrap();
        assert_eq!(
            recovered.state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
        assert_eq!(recovered.revision, 2);
        let (disposition, snapshot) = store.resume("session", 1).unwrap();
        assert_eq!(disposition, ResumeDisposition::WaitingForUser);
        assert_eq!(snapshot.unwrap().revision, 3);
    }

    #[test]
    fn ambiguous_admission_forgets_session_index() {
        let (_temp, store) = store();
        assert!(store.scan_pending("session").unwrap().is_empty());
        store
            .fail_after_metadata_publish
            .store(true, Ordering::Relaxed);
        assert!(store
            .admit_queued_prompt("session", None, None, "first".into(), None, vec![])
            .is_err());
        let pending = store.scan_pending("session").unwrap();
        assert_eq!(pending.len(), 1);
        let next = store
            .admit_queued_prompt("session", None, None, "second".into(), None, vec![])
            .unwrap();
        assert_eq!((next.turn_id, next.queue_sequence), (2, 2));
    }

    #[test]
    fn exhausted_revision_and_sequence_fail_without_persisting_a_mutation() {
        let (temp, store) = store();
        store
            .begin("session", 1, "prompt".into(), None, vec![])
            .unwrap();
        let relative = relative_path("session", 1).unwrap();
        let metadata_before = std::fs::read(temp.path().join("turns").join(&relative)).unwrap();
        {
            let mut cache = store.lock.lock().unwrap();
            let checkpoint = &mut cache.get_mut(&relative).unwrap().checkpoint;
            checkpoint.revision = u64::MAX;
            checkpoint.last_sequence = u64::MAX;
        }
        assert!(store
            .transition(
                "session",
                1,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                None
            )
            .is_err());
        assert!(store.append_event("session", 1, "{}".into()).is_err());
        assert_eq!(
            std::fs::read(temp.path().join("turns").join(relative)).unwrap(),
            metadata_before
        );
        assert_eq!(
            store.load("session", 1).unwrap().state,
            Some(TurnRecoveryStateDto::Running)
        );
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
    fn tool_dispatch_boundary_survives_pause_and_cold_resume() {
        let (temp, store) = store();
        store
            .begin("session", 98, "prompt".into(), None, Vec::new())
            .unwrap();
        store.mark_tool_dispatch_unsafe("session", 98).unwrap();
        store.mark_tool_dispatch_unsafe("session", 98).unwrap();
        let checkpoint = store.load("session", 98).unwrap();
        assert!(!checkpoint.safe_to_resume);
        store
            .transition(
                "session",
                98,
                TurnRecoveryStateDto::PausedRecoverable,
                checkpoint.safe_to_resume,
                Some("background_time_expired".into()),
            )
            .unwrap();
        drop(store);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let (disposition, snapshot) = cold.resume("session", 98).unwrap();
        assert_eq!(disposition, ResumeDisposition::WaitingForUser);
        assert!(!snapshot.unwrap().safe_to_resume);
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
    fn queued_turn_status_fences_uncertain_metadata_and_preserves_member_identity() {
        let (temp, store) = store();
        for id in [11, 12] {
            store
                .admit_queued_prompt(
                    "session",
                    Some(id),
                    Some(id),
                    format!("prompt {id}").into(),
                    None,
                    vec![],
                )
                .unwrap();
            assert_eq!(
                store.queued_turn_status("session", id).unwrap(),
                QueuedTurnStatusDto::Pending
            );
        }
        let batch = store.prepare_mid_turn_batch("session", &[11, 12]).unwrap();
        assert_eq!(
            store.queued_turn_status("session", 12).unwrap(),
            QueuedTurnStatusDto::ClaimedUncertain
        );
        store
            .fail_after_metadata_publish
            .store(true, Ordering::Relaxed);
        assert!(store
            .acknowledge_mid_turn_batch("session", 11, &[11, 12])
            .is_err());
        // The rename is visible but its failed acknowledgement is not enough
        // to issue a receipt until the file and directory can be synced again.
        store
            .fail_before_metadata_sync
            .store(true, Ordering::Relaxed);
        assert!(store.queued_turn_status("session", 12).is_err());
        let expected = QueuedTurnStatusDto::Delivered {
            batch_owner_turn_id: 11,
            member_turn_ids: vec![11, 12],
            transcript_uuid: batch.aggregate_identity.message_id.as_uuid().to_string(),
        };
        assert_eq!(store.queued_turn_status("session", 11).unwrap(), expected);
        assert_eq!(store.queued_turn_status("session", 12).unwrap(), expected);
        assert!(matches!(
            store.queued_turn_status("other", 12),
            Err(DurableTurnStoreError::NotFound { .. })
        ));
        drop(store);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        assert_eq!(cold.queued_turn_status("session", 12).unwrap(), expected);

        cold.admit_queued_prompt(
            "session",
            Some(13),
            Some(13),
            "private".into(),
            None,
            vec![],
        )
        .unwrap();
        cold.cancel_pending("session", 13).unwrap();
        cold.expose_cancelled_pending("session", 13).unwrap();
        assert_eq!(
            cold.queued_turn_status("session", 13).unwrap(),
            QueuedTurnStatusDto::Cancelled
        );
        cold.admit_queued_prompt(
            "session",
            Some(15),
            Some(15),
            "uncertain cancel".into(),
            None,
            vec![],
        )
        .unwrap();
        cold.fail_after_metadata_publish
            .store(true, Ordering::Relaxed);
        assert!(cold.cancel_pending("session", 15).is_err());
        cold.fail_before_metadata_sync
            .store(true, Ordering::Relaxed);
        assert!(cold.queued_turn_status("session", 15).is_err());
        assert_eq!(
            cold.queued_turn_status("session", 15).unwrap(),
            QueuedTurnStatusDto::Cancelled
        );
        cold.begin("session", 14, "started".into(), None, vec![])
            .unwrap();
        cold.cancel("session", 14).unwrap();
        assert_eq!(
            cold.queued_turn_status("session", 14).unwrap(),
            QueuedTurnStatusDto::Terminal {
                state: TurnRecoveryStateDto::Cancelled
            }
        );
    }

    #[test]
    fn claimed_batch_member_is_never_relabelled_as_cancelled_before_claim() {
        for completed in [false, true] {
            let (temp, store) = store();
            for id in [11, 12] {
                store
                    .admit_queued_prompt(
                        "session",
                        Some(id),
                        Some(id),
                        "queued".into(),
                        None,
                        vec![],
                    )
                    .unwrap();
            }
            store.prepare_mid_turn_batch("session", &[11, 12]).unwrap();
            store.claim_legacy_batch_for_test("session", 11).unwrap();
            assert_eq!(
                store.queued_turn_status("session", 12).unwrap(),
                QueuedTurnStatusDto::ClaimedUncertain
            );
            if completed {
                store
                    .transition("session", 11, TurnRecoveryStateDto::Completed, false, None)
                    .unwrap();
            } else {
                store.cancel("session", 11).unwrap();
            }
            drop(store);
            let cold = DurableTurnStore::new(temp.path().join("turns"));
            assert!(cold.cancel_pending("session", 12).unwrap().is_none());
            assert!(matches!(
                cold.cancel("session", 12),
                Err(DurableTurnStoreError::Terminal { .. })
            ));
            assert!(!cold.load("session", 12).unwrap().cancelled_before_claim);
            assert_eq!(
                cold.queued_turn_status("session", 12).unwrap(),
                QueuedTurnStatusDto::ClaimedUncertain
            );
        }
    }

    #[test]
    fn interrupted_midturn_batch_is_never_requeued_after_reopen() {
        for acknowledged in [false, true] {
            let (temp, store) = store();
            for id in [21, 22] {
                store
                    .admit_queued_prompt(
                        "session",
                        Some(id),
                        Some(id),
                        "queued".into(),
                        None,
                        vec![],
                    )
                    .unwrap();
            }
            let batch = store.prepare_mid_turn_batch("session", &[21, 22]).unwrap();
            if acknowledged {
                store
                    .acknowledge_mid_turn_batch("session", 21, &[21, 22])
                    .unwrap();
            }
            drop(store);
            let cold = DurableTurnStore::new(temp.path().join("turns"));
            assert!(cold.scan_pending("session").unwrap().is_empty());
            for id in [21, 22] {
                assert!(!cold.load("session", id).unwrap().cancelled_before_claim);
                assert!(cold.cancel_pending("session", id).unwrap().is_none());
                let expected = if acknowledged {
                    QueuedTurnStatusDto::Delivered {
                        batch_owner_turn_id: 21,
                        member_turn_ids: vec![21, 22],
                        transcript_uuid: batch.aggregate_identity.message_id.as_uuid().to_string(),
                    }
                } else {
                    QueuedTurnStatusDto::ClaimedUncertain
                };
                assert_eq!(cold.queued_turn_status("session", id).unwrap(), expected);
                assert!(cold.claim_pending("session", id).is_err());
            }
            if !acknowledged {
                assert!(cold.cancel("session", 21).is_err());
                assert!(cold.cancel("session", 22).is_err());
                assert_eq!(
                    cold.resume("session", 21).unwrap().0,
                    ResumeDisposition::WaitingForUser
                );
                assert_eq!(
                    cold.resume("session", 22).unwrap().0,
                    ResumeDisposition::WaitingForUser
                );
            }
        }
    }

    #[test]
    fn midturn_claim_write_failure_never_exposes_an_unfenced_batch() {
        for after_publish in [false, true] {
            let (temp, store) = store();
            for id in [31, 32] {
                store
                    .admit_queued_prompt(
                        "session",
                        Some(id),
                        Some(id),
                        format!("input {id}").into(),
                        None,
                        vec![],
                    )
                    .unwrap();
            }
            if after_publish {
                store
                    .fail_after_metadata_publish
                    .store(true, Ordering::Relaxed);
            } else {
                store
                    .fail_before_metadata_write
                    .store(true, Ordering::Relaxed);
            }
            assert!(store.prepare_mid_turn_batch("session", &[31, 32]).is_err());
            drop(store);
            let cold = DurableTurnStore::new(temp.path().join("turns"));
            let ids = cold
                .scan_pending("session")
                .unwrap()
                .into_iter()
                .map(|entry| entry.turn_id)
                .collect::<Vec<_>>();
            if after_publish {
                assert!(ids.is_empty());
                assert_eq!(
                    cold.queued_turn_status("session", 32).unwrap(),
                    QueuedTurnStatusDto::ClaimedUncertain
                );
                assert!(cold.prepare_mid_turn_batch("session", &[31, 32]).is_err());
                assert!(cold.claim_pending("session", 31).is_err());
            } else {
                assert_eq!(ids, vec![31, 32]);
                assert_eq!(
                    cold.queued_turn_status("session", 32).unwrap(),
                    QueuedTurnStatusDto::Pending
                );
            }
            assert_eq!(
                cold.load("session", 32).unwrap().prompt.string_at(""),
                Some("input 32")
            );
        }
    }

    #[test]
    fn uncertain_coalesced_batch_retains_last_member_identity_and_ordered_images() {
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
        let pending = cold.scan_pending("session").expect("scan after claim");
        assert!(
            pending.is_empty(),
            "claimed inputs are not replayed automatically"
        );
        let persisted = cold.load("session", first.turn_id).unwrap();
        assert_eq!(
            persisted.queue_delivery,
            QueueDeliveryState::CoalescedClaimed
        );
        assert_eq!(persisted.coalesced_batch, Some(batch.clone()));
        let inputs = persisted
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

        assert_eq!(
            cold.queued_turn_status("session", second.turn_id).unwrap(),
            QueuedTurnStatusDto::ClaimedUncertain
        );
    }

    #[test]
    fn slash_and_skill_first_run_keep_the_restored_client_turn_token() {
        let (temp, store) = store();
        for (turn_id, skill) in [
            (101, None),
            (
                102,
                Some(DurableSkillPrompt::new(
                    "/skill".into(),
                    "expanded body".into(),
                    Vec::new(),
                )),
            ),
        ] {
            store
                .admit_queued_slash(
                    "session",
                    Some(turn_id),
                    Some(turn_id + 1),
                    "/skill".into(),
                    vec![ImageRefDto {
                        media_type: "image/png".into(),
                        base64: "AA==".into(),
                    }],
                )
                .unwrap();
            store.claim_queued_slash("session", turn_id).unwrap();
            let checkpoint = store
                .prepare_claimed_slash_turn("session", turn_id, "expanded".into(), skill)
                .unwrap();
            let inputs = checkpoint.initial_prompt_inputs().unwrap();
            assert_eq!(
                inputs[0].images.len(),
                1,
                "slash/skill images survive correlation"
            );
            assert_eq!(
                inputs[0].transcript_row_token,
                Some(format!("client-turn:{}", turn_id + 1))
            );
            assert!(
                inputs
                    .iter()
                    .skip(1)
                    .all(|input| input.transcript_row_token.is_none()),
                "expanded/meta skill rows never impersonate the user command"
            );
            let cold = DurableTurnStore::new(temp.path().join("turns"));
            let restored = cold
                .load("session", turn_id)
                .unwrap()
                .initial_prompt_inputs()
                .unwrap();
            assert_eq!(restored[0].message_id, inputs[0].message_id);
            assert_eq!(restored[0].images.len(), 1, "images survive cold restore");
            assert_eq!(
                restored[0].transcript_row_token,
                inputs[0].transcript_row_token
            );
        }
    }

    #[test]
    fn direct_and_restored_prompt_inputs_keep_the_original_client_turn_token() {
        let (temp, store) = store();
        let checkpoint = store
            .admit_queued_prompt(
                "session",
                Some(101),
                Some(7),
                Utf16JsonProjection::from("same prompt"),
                None,
                Vec::new(),
            )
            .unwrap();
        let identity = checkpoint.prompt_identity.as_ref().unwrap();
        let mut inputs = vec![
            orchestrator::QueuedPromptInput::default(),
            identity.queued_prompt_input("same prompt".into(), &[]),
        ];
        checkpoint.correlate_prompt_inputs(&mut inputs);
        assert_eq!(
            inputs[0].transcript_row_token, None,
            "never bind an unrelated row"
        );
        assert_eq!(
            inputs[1].transcript_row_token.as_deref(),
            Some("client-turn:7")
        );
        inputs[1].transcript_row_token = Some("client-turn:7,8".into());
        checkpoint.correlate_prompt_inputs(&mut inputs);
        assert_eq!(
            inputs[1].transcript_row_token.as_deref(),
            Some("client-turn:7,8"),
            "preserve coalesced correlators"
        );
        drop(store);
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let restored = cold.load("session", checkpoint.turn_id).unwrap();
        let mut inputs = vec![restored
            .prompt_identity
            .as_ref()
            .unwrap()
            .queued_prompt_input(restored.prompt.clone(), &restored.images)];
        restored.correlate_prompt_inputs(&mut inputs);
        assert_eq!(
            inputs[0].transcript_row_token.as_deref(),
            Some("client-turn:7")
        );
    }

    #[test]
    fn queued_prompt_client_turn_token_preserves_correlators_across_cold_coalescing() {
        let (temp, store) = store();
        let members = [(101, Some(7)), (102, None), (103, Some(8))]
            .into_iter()
            .map(|(turn_id, client_turn_id)| {
                store
                    .admit_queued_prompt(
                        "session",
                        Some(turn_id),
                        client_turn_id,
                        Utf16JsonProjection::from("identical prompt"),
                        None,
                        Vec::new(),
                    )
                    .expect("admit independently correlated prompt")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            members[0].queued_prompt_inputs(&store).unwrap()[0]
                .transcript_row_token
                .as_deref(),
            Some("client-turn:7"),
            "an unbatched prompt uses the original client ID instead of the durable ID"
        );
        assert_eq!(
            members[1].queued_prompt_inputs(&store).unwrap()[0]
                .transcript_row_token
                .as_deref(),
            Some("client-turn:102"),
            "missing original correlators fall back to the allocated durable ID"
        );
        let ids = members
            .iter()
            .map(|member| member.turn_id)
            .collect::<Vec<_>>();
        let batch = store.prepare_mid_turn_batch("session", &ids).unwrap();
        drop(store);

        let cold = DurableTurnStore::new(temp.path().join("turns"));
        assert!(cold.scan_pending("session").unwrap().is_empty());
        let restored = cold
            .load("session", ids[0])
            .expect("retain aggregate owner");
        let inputs = restored.queued_prompt_inputs(&cold).unwrap();
        assert_eq!(
            inputs.len(),
            1,
            "a coalesced group remains one persisted row"
        );
        assert_eq!(
            inputs[0].text.string_at(""),
            Some("identical prompt\nidentical prompt\nidentical prompt")
        );
        assert_eq!(
            inputs[0].message_id,
            Some(batch.aggregate_identity.message_id)
        );
        assert_eq!(inputs[0].transcript_row_token.as_deref(), Some("client-turn:7,102,8"), "the token retains each admitted correlator in order, independent of duplicate prompt text");
        assert_eq!(
            restored.queued_prompt_inputs(&cold).unwrap()[0].transcript_row_token,
            inputs[0].transcript_row_token,
            "repeated reconstruction keeps the exact same correlation token"
        );
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
                assert!(store.scan_pending("session").unwrap().is_empty());
                assert_eq!(
                    store.queued_turn_status("session", second_id).unwrap(),
                    QueuedTurnStatusDto::ClaimedUncertain
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
        assert!(store.scan_pending("session").unwrap().is_empty());
        assert_eq!(
            store.queued_turn_status("session", second.turn_id).unwrap(),
            QueuedTurnStatusDto::ClaimedUncertain
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
            cancelled_before_claim: false,
            coalesced_batch: None,
            revision: 2,
            state: Some(TurnRecoveryStateDto::Running),
            first_sequence: 1,
            last_sequence: 1,
            safe_to_resume: true,
            projection_uncertain: false,
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
        assert_eq!(metadata.revision, 2);
        assert_eq!(transitioned.revision, metadata.revision);
    }

    #[test]
    fn transition_returns_the_committed_retention_window_inside_compaction_slack() {
        let (temp, store) = store();
        store
            .begin("session", 75, "prompt".into(), None, Vec::new())
            .unwrap();
        for _ in 0..=MAX_RETAINED_EVENTS {
            store
                .append_event("session", 75, r#"{"type":"text_delta","text":"x"}"#.into())
                .unwrap();
        }
        let snapshot = store
            .transition(
                "session",
                75,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                None,
            )
            .unwrap();
        assert_eq!(snapshot.first_sequence, 2);
        assert_eq!(snapshot.last_sequence, (MAX_RETAINED_EVENTS + 1) as u64);
        let warm = store.load("session", 75).unwrap().snapshot();
        let cold = DurableTurnStore::new(temp.path().join("turns"))
            .load("session", 75)
            .unwrap()
            .snapshot();
        assert_eq!(snapshot, warm);
        assert_eq!(snapshot, cold);
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
        assert_eq!(recovered.revision, 2);
        assert!(!recovered.safe_to_resume);
        let metadata: StoredTurnMetadata = serde_json::from_slice(
            &std::fs::read(temp.path().join("turns/session-a/72.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata.revision, 2);
        let second_cold = DurableTurnStore::new(temp.path().join("turns"));
        assert_eq!(second_cold.load("session-a", 72).unwrap().revision, 2);
        assert_eq!(
            recovered_store
                .resume("session-a", 72)
                .expect("resume gate")
                .0,
            ResumeDisposition::Terminal
        );
    }

    #[test]
    fn committed_terminal_metadata_is_not_rewritten_from_event_stop_reason() {
        let (temp, store) = store();
        store
            .begin("session", 73, "prompt".into(), None, Vec::new())
            .unwrap();
        store
            .append_event(
                "session",
                73,
                r#"{"type":"turn_ended","outcome":{"type":"max_turns"},"stop_reason":"max_tokens"}"#.into(),
            )
            .unwrap();
        let saved = store
            .transition(
                "session",
                73,
                TurnRecoveryStateDto::Failed,
                false,
                Some("host_specific_reason".into()),
            )
            .unwrap();
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let recovered = cold.load("session", 73).unwrap();
        assert_eq!(recovered.revision, saved.revision);
        assert_eq!(recovered.reason.as_deref(), Some("host_specific_reason"));
    }

    #[test]
    fn journal_only_max_turns_uses_the_same_reason_as_the_live_transition() {
        let (temp, store) = store();
        store
            .begin("session", 731, "prompt".into(), None, Vec::new())
            .unwrap();
        store
            .append_event(
                "session",
                731,
                r#"{"type":"turn_ended","outcome":{"type":"max_turns"},"stop_reason":"max_tokens"}"#.into(),
            )
            .unwrap();
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let recovered = cold.load("session", 731).unwrap();
        assert_eq!(recovered.state, Some(TurnRecoveryStateDto::Failed));
        assert_eq!(recovered.reason.as_deref(), Some("max_turns"));
        assert_eq!(recovered.revision, 2);
    }

    #[test]
    fn error_with_an_isolated_utf16_surrogate_is_durable_and_recovers_terminal() {
        let (temp, store) = store();
        store
            .begin("session", 74, "prompt".into(), None, Vec::new())
            .unwrap();
        let event = r#"{"type":"error","message":"fail \ud800"}"#;
        assert!(event_requires_durable_flush(event));
        store.append_event("session", 74, event.into()).unwrap();
        let cold = DurableTurnStore::new(temp.path().join("turns"));
        let recovered = cold.load("session", 74).unwrap();
        assert_eq!(recovered.state, Some(TurnRecoveryStateDto::Failed));
        assert_eq!(recovered.reason.as_deref(), Some("fail �"));
        assert_eq!(recovered.revision, 2);
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
            cancelled_before_claim: false,
            coalesced_batch: None,
            revision: 1,
            state: Some(TurnRecoveryStateDto::Running),
            first_sequence: events.first().expect("retained event").sequence,
            last_sequence: events.last().expect("retained event").sequence,
            safe_to_resume: true,
            projection_uncertain: false,
            reason: None,
            events,
        };

        assert_eq!(checkpoint.first_sequence, 2);
        assert!(checkpoint.has_replay_gap(None));
        assert!(checkpoint.has_replay_gap(Some(0)));
        assert!(!checkpoint.has_replay_gap(Some(1)));
        assert!(checkpoint.has_replay_gap(Some(checkpoint.last_sequence + 1)));
        assert_eq!(
            checkpoint.replay_gap_reason(Some(checkpoint.last_sequence + 1)),
            Some("replay_cursor_ahead")
        );
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
