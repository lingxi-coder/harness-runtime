//! Consume-once producer results live outside model history until delivered.
//! The metadata checkpoint is host recovery state, never a prompt attachment.

use super::ConversationOrchestrator;
use crate::prompt::memory_update::{MemoryUpdateSource, PendingMemoryUpdate};
use hooks::attachment::HookPublicationGuard;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection as Source, ConversationMessage, MessageId, SessionId,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(crate) const METADATA_KIND: &str = "pending-transient-reminders";
static REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct ConsumedReminder {
    pub(crate) kind: &'static str,
    pub(crate) message: ConversationMessage,
    pub(crate) hook_event: Option<String>,
    pub(crate) publication_guard: Option<Arc<dyn HookPublicationGuard>>,
}

#[cfg(test)]
#[path = "consumed_reminder_recovery_tests.rs"]
mod tests;

impl ConsumedReminder {
    pub(crate) fn memory(message: ConversationMessage) -> Self {
        Self {
            kind: "memory_update",
            message,
            hook_event: None,
            publication_guard: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct ConsumedReminderState {
    owner: Option<SessionId>,
    source_path: Option<std::path::PathBuf>,
    revision: u64,
    persisted_revision: u64,
    authority_floor: u64,
    rows: Vec<ConsumedReminder>,
    unrendered_memory_updates: Vec<PendingMemoryUpdate>,
    memory_scan_floor: Option<i64>,
}

#[derive(Clone)]
struct LiveConsumedReminderSnapshot {
    session: SessionId,
    source_path: Option<std::path::PathBuf>,
    source_authority: Option<session::jsonl::writer::TranscriptFileAuthority>,
    revision: u64,
    rows: Vec<ConsumedReminder>,
    unrendered_memory_updates: Vec<PendingMemoryUpdate>,
    memory_scan_floor: Option<i64>,
}

impl ConsumedReminderState {
    fn bind(&mut self, session: SessionId) {
        if self.owner != Some(session) {
            *self = Self {
                owner: Some(session),
                ..Default::default()
            };
        }
    }

    fn changed(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let floor = self
            .revision
            .max(u64::try_from(now).unwrap_or(u64::MAX - 1));
        let previous = REVISION
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                Some(old.max(floor).saturating_add(1))
            })
            .expect("infallible revision update");
        self.revision = previous.max(floor).saturating_add(1);
        self.authority_floor = self.revision;
    }

    fn checkpoint(&self, session: SessionId) -> Source {
        Source::from_serde_value(&serde_json::json!({
            "type": METADATA_KIND,
            "sessionId": session.as_uuid().to_string(),
            "revision": self.revision,
            "reminders": self.rows.iter().map(|row| serde_json::json!({
                "kind": row.kind,
                "message": row.message,
                "hookEvent": row.hook_event,
                "guarded": row.publication_guard.is_some(),
            })).collect::<Vec<_>>(),
            // The legacy producer is drained before its asynchronous directory
            // scan. Preserve its raw queue until rendered rows replace it.
            "unrenderedMemoryUpdates": self.unrendered_memory_updates.iter().map(|update| serde_json::json!({
                "source": match update.source {
                    MemoryUpdateSource::Dream => "dream",
                    MemoryUpdateSource::Extract => "extract",
                    MemoryUpdateSource::Other => "other",
                    MemoryUpdateSource::SyncUnsaved => "sync_unsaved",
                },
                "summaryJson": update.summary.to_json_string().expect("queued source is exact JSON"),
            })).collect::<Vec<_>>(),
            "memoryScanFloor": self.memory_scan_floor,
        }))
        .expect("host recovery metadata serializes")
    }
}

impl ConversationOrchestrator {
    pub(crate) fn export_consumed_reminder_authority(
        &self,
    ) -> Option<lingxi_core::host::ResumeTransientReminderAuthority> {
        // A transfer snapshot is taken after the old turn has settled. Hosts
        // retire that owner before installing this bag on a rebuilt runtime.
        // Capturing an active request would permit a late old-owner ACK.
        let _idle = self.turn_gate.try_lock().ok()?;
        let pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(lingxi_core::host::ResumeTransientReminderAuthority::new(
            LiveConsumedReminderSnapshot {
                session: pending.owner?,
                source_path: pending.source_path.clone(),
                source_authority: self
                    .transcript
                    .jsonl_writer
                    .as_ref()
                    .and_then(|writer| writer.active_transcript_authority()),
                revision: pending.revision,
                rows: pending.rows.clone(),
                unrendered_memory_updates: pending.unrendered_memory_updates.clone(),
                memory_scan_floor: pending.memory_scan_floor,
            },
        ))
    }

    pub(crate) fn import_consumed_reminder_authority(
        &self,
        session: SessionId,
        authority: &lingxi_core::host::ResumeTransientReminderAuthority,
    ) {
        let Some(snapshot) = authority.get::<LiveConsumedReminderSnapshot>() else {
            return;
        };
        let path = self.transcript.jsonl_writer.as_ref().map(|writer| {
            writer
                .session_target_path(session)
                .unwrap_or_else(|| writer.active_path())
        });
        if snapshot.session != session || snapshot.source_path != path {
            return;
        }
        let authority_matches = match (
            &snapshot.source_authority,
            self.transcript.jsonl_writer.as_ref(),
        ) {
            (Some(source), Some(writer)) => writer.verify_active_transcript_source(source).is_ok(),
            (None, Some(writer)) => writer.active_transcript_authority().is_none(),
            (None, None) => true,
            _ => false,
        };
        if !authority_matches {
            return;
        }
        let mut pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.bind(session);
        if snapshot.revision < pending.authority_floor {
            return;
        }
        pending.source_path = path;
        pending.rows = snapshot
            .rows
            .iter()
            .filter(|row| {
                row.publication_guard
                    .as_ref()
                    .is_none_or(|guard| guard.is_current())
            })
            .cloned()
            .collect();
        pending.revision = snapshot.revision;
        pending.unrendered_memory_updates = snapshot.unrendered_memory_updates.clone();
        pending.memory_scan_floor = snapshot.memory_scan_floor;
        pending.changed();
        self.restore_unrendered_memory_queue(&pending);
    }

    /// Rebuild a live runtime with an explicitly transferred, opaque authority
    /// captured from its prior owner. Loading JSONL alone remains a cold mount.
    /// The prior owner must stay idle and be retired before this transfer.
    #[must_use]
    pub fn with_pending_transient_reminder_authority(
        self,
        authority: Option<lingxi_core::host::ResumeTransientReminderAuthority>,
    ) -> Self {
        if let Some(authority) = authority {
            let session = self
                .session
                .try_lock()
                .expect("builder has no active turn")
                .session_id;
            self.import_consumed_reminder_authority(session, &authority);
        }
        self
    }

    pub(crate) fn retain_consumed_reminders(
        &self,
        session: SessionId,
        rows: Vec<ConsumedReminder>,
    ) {
        if rows.is_empty() {
            return;
        }
        let mut pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.bind(session);
        pending.source_path = self.transcript.jsonl_writer.as_ref().map(|writer| {
            writer
                .session_target_path(session)
                .unwrap_or_else(|| writer.active_path())
        });
        pending.rows.extend(rows);
        pending.changed();
    }

    /// Snapshot without holding the producer mutex across persistence. At the
    /// drain boundary this runs before any cancellable work can lose the source.
    pub(crate) fn retain_unrendered_memory_updates(&self, session: SessionId) {
        let updates = self
            .prompt_runtime
            .pending_memory_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let floor = self
            .prompt_runtime
            .last_memory_scan_ms
            .load(Ordering::Relaxed);
        let mut pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.bind(session);
        if pending.unrendered_memory_updates == updates
            && (pending.memory_scan_floor == Some(floor)
                || updates.is_empty() && pending.memory_scan_floor.is_none())
        {
            return;
        }
        pending.source_path = self.transcript.jsonl_writer.as_ref().map(|writer| {
            writer
                .session_target_path(session)
                .unwrap_or_else(|| writer.active_path())
        });
        pending.unrendered_memory_updates = updates;
        pending.memory_scan_floor = Some(floor);
        pending.changed();
    }

    fn restore_unrendered_memory_queue(&self, pending: &ConsumedReminderState) {
        *self
            .prompt_runtime
            .pending_memory_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            pending.unrendered_memory_updates.clone();
        if let Some(floor) = pending.memory_scan_floor {
            self.prompt_runtime
                .last_memory_scan_ms
                .store(floor, Ordering::Relaxed);
        }
    }

    pub(crate) fn consumed_reminders_for_kind(
        &self,
        session: SessionId,
        kind: &str,
    ) -> Vec<ConsumedReminder> {
        let mut pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.bind(session);
        let count = pending.rows.len();
        pending.rows.retain(|row| {
            row.publication_guard
                .as_ref()
                .is_none_or(|guard| guard.is_current())
        });
        if pending.rows.len() != count {
            pending.changed();
        }
        pending
            .rows
            .iter()
            .filter(|row| row.kind == kind)
            .cloned()
            .collect()
    }

    /// Capture before spawning. The owned persistence gate remains in the task
    /// after caller cancellation; version checks reject an older queued snapshot
    /// even if a newer ACK reaches the writer first.
    pub(crate) async fn persist_consumed_reminders(&self, session: SessionId) {
        let (revision, checkpoint) = {
            let pending = self
                .prompt_runtime
                .consumed_reminders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.owner != Some(session) || pending.revision == pending.persisted_revision {
                return;
            }
            (pending.revision, pending.checkpoint(session))
        };
        let Some(writer) = self.transcript.jsonl_writer.clone() else {
            return;
        };
        let Some(authority) = writer.capture_pending_transient_reminder_target(session) else {
            return;
        };
        let persist_gate = Arc::clone(&self.prompt_runtime.consumed_reminder_persistence);
        let state = Arc::clone(&self.prompt_runtime.consumed_reminders);
        let _commit = crate::request_context::preparation_commit_scope();
        let task = tokio::spawn(async move {
            let _owned_gate = persist_gate.lock_owned().await;
            writer
                .append_pending_transient_reminders(authority, checkpoint)
                .await?;
            let mut pending = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.owner == Some(session) {
                pending.persisted_revision = pending.persisted_revision.max(revision);
            }
            Ok::<_, session::jsonl::writer::WriterError>(())
        });
        let error = match task.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(error) = error {
            self.record_transcript_append_failure(
                &session.as_uuid().to_string(),
                "pending_transient_reminders",
                &error,
            )
            .await;
        }
    }

    pub(crate) fn consumed_reminder_ids_in_request(
        &self,
        request: &[ConversationMessage],
    ) -> Vec<MessageId> {
        let pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .rows
            .iter()
            .filter(|row| {
                request
                    .iter()
                    .any(|message| message.id() == row.message.id())
            })
            .map(|row| row.message.id())
            .collect()
    }

    /// This is also used for explicit prompt.attachment suppression and expired
    /// source authority. Synthetic turn.step answers never call this ACK seam.
    pub(crate) async fn acknowledge_consumed_reminder_ids(&self, ids: &[MessageId]) {
        if ids.is_empty() {
            return;
        }
        let session = {
            let mut pending = self
                .prompt_runtime
                .consumed_reminders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let count = pending.rows.len();
            pending.rows.retain(|row| !ids.contains(&row.message.id()));
            if pending.rows.len() == count {
                return;
            }
            pending.changed();
            pending.owner
        };
        if let Some(session) = session {
            self.persist_consumed_reminders(session).await;
        }
    }

    pub(crate) async fn acknowledge_consumed_reminders_in_request(
        &self,
        request: &[ConversationMessage],
    ) {
        self.acknowledge_consumed_reminder_ids(&self.consumed_reminder_ids_in_request(request))
            .await;
    }

    /// Exact host metadata, not text guessed from the rendered model history.
    /// A same-process remount retains real live guards. A cold process cannot
    /// manufacture the retired executor authority of a guarded completion.
    pub(crate) fn restore_consumed_reminders_metadata(
        &self,
        session: SessionId,
        metadata: &[Source],
    ) {
        let latest = metadata
            .iter()
            .filter(|row| {
                row.string_at("/type") == Some(METADATA_KIND)
                    && row
                        .string_at("/sessionId")
                        .and_then(SessionId::parse_prefixed)
                        == Some(session)
            })
            .filter_map(|row| {
                let value: serde_json::Value =
                    serde_json::from_str(&row.to_json_string().ok()?).ok()?;
                Some((value.get("revision")?.as_u64()?, value))
            })
            .max_by_key(|(revision, _)| *revision);
        let mut pending = self
            .prompt_runtime
            .consumed_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self.transcript.jsonl_writer.as_ref().map(|writer| {
            writer
                .session_target_path(session)
                .unwrap_or_else(|| writer.active_path())
        });
        if pending.owner == Some(session) && pending.source_path != path {
            *pending = ConsumedReminderState::default();
        }
        pending.bind(session);
        pending.source_path = path;
        let Some((revision, value)) = latest else {
            self.restore_unrendered_memory_queue(&pending);
            return;
        };
        if pending.revision >= revision {
            self.restore_unrendered_memory_queue(&pending);
            return;
        }
        let mut rows = Vec::new();
        let Some(records) = value.get("reminders").and_then(serde_json::Value::as_array) else {
            return;
        };
        let mut filtered = false;
        for record in records {
            let kind = match record.get("kind").and_then(serde_json::Value::as_str) {
                Some("memory_update") => "memory_update",
                Some("async_hook_response") => "async_hook_response",
                _ => continue,
            };
            let Some(message) = record
                .get("message")
                .cloned()
                .and_then(|value| serde_json::from_value::<ConversationMessage>(value).ok())
            else {
                continue;
            };
            if !matches!(message, ConversationMessage::User { is_meta: true, .. }) {
                continue;
            }
            let guarded = record
                .get("guarded")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let live = pending
                .rows
                .iter()
                .find(|row| row.message.id() == message.id())
                .cloned();
            let publication_guard = live
                .and_then(|row| row.publication_guard)
                .filter(|guard| guard.is_current());
            if guarded && publication_guard.is_none() {
                filtered = true;
                continue;
            }
            rows.push(ConsumedReminder {
                kind,
                message,
                hook_event: record
                    .get("hookEvent")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                publication_guard,
            });
        }
        pending.revision = revision;
        pending.persisted_revision = revision;
        pending.authority_floor = revision;
        pending.rows = rows;
        pending.unrendered_memory_updates = value
            .get("unrenderedMemoryUpdates")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|record| {
                Some(PendingMemoryUpdate {
                    source: MemoryUpdateSource::from_native(record.get("source")?.as_str()?),
                    summary: Source::parse(record.get("summaryJson")?.as_str()?).ok()?,
                })
            })
            .take(Self::MAX_PENDING_MEMORY_UPDATES)
            .collect();
        pending.memory_scan_floor = value
            .get("memoryScanFloor")
            .and_then(serde_json::Value::as_i64);
        self.restore_unrendered_memory_queue(&pending);
        if filtered {
            pending.changed();
            // A cold loader cannot verify an executor; that absence is not a
            // newer delivery receipt which invalidates an explicit live bag.
            pending.authority_floor = revision;
        }
    }
}
