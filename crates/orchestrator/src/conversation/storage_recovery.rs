use super::*;
use session::JsonlMessage;

const DISK_FULL_PAUSE: &str = "Session paused because disk space is insufficient (ENOSPC). Free disk space, then retry in this same session. Existing history is retained; pending records will be saved before continuing.";

#[derive(Default)]
pub(crate) struct StoragePause {
    rows: std::collections::VecDeque<JsonlMessage>,
}

impl ConversationOrchestrator {
    pub(crate) async fn pause_for_disk_full(&self) {
        let session = self.session.lock().await.session_id.as_uuid().to_string();
        self.mark_storage_paused(session).await;
    }

    pub(super) async fn mark_storage_paused(&self, session: String) {
        let active = self.session.lock().await.session_id.as_uuid().to_string() == session;
        let newly_paused = {
            let mut pauses = self.transcript.storage_pauses.lock().await;
            if let std::collections::hash_map::Entry::Vacant(entry) = pauses.entry(session) {
                entry.insert(StoragePause::default());
                true
            } else {
                false
            }
        };
        if newly_paused && active {
            self.output.emit_session_storage_state(true).await;
            self.output
                .emit_system_notice(&DISK_FULL_PAUSE.into(), true)
                .await;
        }
    }

    /// Keep the exact native row, including its UUID and provider metadata.
    /// Advancing the logical tip lets later retained rows preserve their chain;
    /// callers still receive the write error and never claim a durable append.
    async fn retain_storage_row(&self, row: &JsonlMessage) {
        self.mark_storage_paused(row.session_id.clone()).await;
        let mut pauses = self.transcript.storage_pauses.lock().await;
        let pause = pauses.entry(row.session_id.clone()).or_default();
        if !pause.rows.iter().any(|pending| pending.uuid == row.uuid) {
            pause.rows.push_back(row.clone());
        }
        *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
    }

    pub(crate) async fn append_transcript_row(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
    ) -> Result<(), session::jsonl::writer::WriterError> {
        let _append = self.transcript.storage_append_lock.lock().await;
        let pending = self
            .transcript
            .storage_pauses
            .lock()
            .await
            .get(&row.session_id)
            .is_some_and(|pause| !pause.rows.is_empty());
        if pending {
            self.retain_storage_row(row).await;
            return Err(lingxi_core::host::FsError::Io(DISK_FULL_PAUSE.into()).into());
        }
        let result = writer.append(row).await;
        if result
            .as_ref()
            .is_err_and(|error| cost::persistence::is_disk_full(&error.to_string()))
        {
            self.retain_storage_row(row).await;
        }
        result
    }

    pub(crate) async fn append_pending_transcript_row(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
    ) -> Result<
        (session::jsonl::TranscriptAppendOutcome, Option<String>),
        session::jsonl::TranscriptWriterError,
    > {
        let _append = self.transcript.storage_append_lock.lock().await;
        let result = writer.append_pending_prompt_message_once(row).await;
        if result
            .as_ref()
            .is_err_and(|error| cost::persistence::is_disk_full(&error.to_string()))
        {
            self.retain_storage_row(row).await;
        }
        result
    }

    pub(crate) async fn check_storage_pause(&self) -> Result<(), OrchestratorError> {
        let session = self.session.lock().await.session_id.as_uuid().to_string();
        if self
            .transcript
            .storage_pauses
            .lock()
            .await
            .contains_key(&session)
        {
            return Err(OrchestratorError::Internal(DISK_FULL_PAUSE.into()));
        }
        Ok(())
    }

    /// A new human turn is the retry boundary. Never replace the session or
    /// replay tools: settle retained usage and append original transcript rows.
    pub(crate) async fn recover_session_storage(&self) -> Result<(), OrchestratorError> {
        let session = self.session.lock().await.session_id.as_uuid().to_string();
        let _append = self.transcript.storage_append_lock.lock().await;
        let mut pauses = self.transcript.storage_pauses.lock().await;
        let Some(pause) = pauses.get_mut(&session) else {
            return Ok(());
        };
        if let Some(tracker) = self.model_runtime.cost_tracker.as_ref() {
            cost::CostSessionScope::new(tracker.clone())
                .recover_disk_full()
                .await
                .map_err(|error| {
                    OrchestratorError::Internal(format!(
                        "{DISK_FULL_PAUSE} Recovery failed: {error}"
                    ))
                })?;
        }
        if !pause.rows.is_empty() {
            let writer = self.transcript.jsonl_writer.as_ref().ok_or_else(|| {
                OrchestratorError::Internal("storage recovery has no transcript writer".into())
            })?;
            while let Some(row) = pause.rows.front() {
                let (_, is_tip) =
                    writer
                        .append_native_message_once(row)
                        .await
                        .map_err(|error| {
                            OrchestratorError::Internal(format!(
                                "{DISK_FULL_PAUSE} History could not be saved: {error}"
                            ))
                        })?;
                if is_tip {
                    *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
                }
                pause.rows.pop_front();
            }
        }
        pauses.remove(&session);
        drop(pauses);
        self.output.emit_session_storage_state(false).await;
        self.output.emit_system_notice(&"Disk space is available again. Pending records were saved; continuing this session with its existing history.".into(), false).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use std::path::PathBuf;

    fn fixture() -> (
        tempfile::TempDir,
        ConversationOrchestrator,
        Arc<session::jsonl::JsonlWriter>,
        PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = dir.path().join(format!("{}.jsonl", id.as_uuid()));
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        ));
        writer.activate_session_target_with_durable_lock(
            id,
            path.clone(),
            dir.path().to_path_buf(),
            Arc::new(session::jsonl::DurableTranscriptWriter::open(&state).unwrap()),
        );
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer.clone())
        .with_session_id(id);
        (dir, orch, writer, path)
    }

    async fn row(orch: &ConversationOrchestrator, text: &str) -> JsonlMessage {
        JsonlMessage::from_projection(&Utf16JsonProjection::parse(&serde_json::json!({
            "type": "user", "uuid": uuid::Uuid::new_v4().to_string(),
            "parentUuid": orch.transcript.last_jsonl_uuid.lock().await.clone(),
            "isSidechain": false, "sessionId": orch.session.lock().await.session_id.as_uuid().to_string(),
            "timestamp": "2026-10-23T04:11:27.000Z", "cwd": "/workspace", "version": "0.12.0",
            "userType": "external", "entrypoint": "cli", "message": {"role": "user", "content": [{"type": "text", "text": text}]}
        }).to_string()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn disk_full_recovery_preserves_history_and_appends_buffered_rows_once() {
        let (_dir, orch, writer, path) = fixture();
        let saved = row(&orch, "saved history").await;
        writer.append(&saved).await.unwrap();
        *orch.transcript.last_jsonl_uuid.lock().await = Some(saved.uuid.clone());
        let prefix = std::fs::read(&path).unwrap();
        let first = row(&orch, "unsaved tool result").await;
        orch.retain_storage_row(&first).await;
        let second = row(&orch, "following attachment").await;
        orch.retain_storage_row(&second).await;
        assert_eq!(second.parent_uuid.as_deref(), Some(first.uuid.as_str()));
        assert!(orch.capture_main_output().await.is_err());
        orch.recover_session_storage().await.unwrap();
        orch.recover_session_storage().await.unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(&prefix));
        let rows = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let messages = rows
            .iter()
            .filter(|row| row.get("uuid").is_some())
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["uuid"], first.uuid);
        assert_eq!(messages[2]["uuid"], second.uuid);
        assert_eq!(messages[2]["parentUuid"], first.uuid);
        assert!(orch.check_storage_pause().await.is_ok());
    }

    #[tokio::test]
    async fn disk_full_recovery_retains_pending_history_when_retry_fails() {
        let (_dir, orch, writer, path) = fixture();
        let pending = row(&orch, "keep this result").await;
        orch.retain_storage_row(&pending).await;
        std::fs::create_dir(&path).unwrap();
        assert!(orch.recover_session_storage().await.is_err());
        assert!(orch.check_storage_pause().await.is_err());
        assert_eq!(
            orch.transcript
                .storage_pauses
                .lock()
                .await
                .values()
                .next()
                .unwrap()
                .rows
                .len(),
            1
        );
        std::fs::remove_dir(&path).unwrap();
        orch.recover_session_storage().await.unwrap();
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("keep this result"));
        // Already-landed rows use the same immutable UUID, so an uncertain
        // acknowledgement and another user retry cannot duplicate history.
        orch.retain_storage_row(&pending).await;
        orch.recover_session_storage().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), persisted);
        assert!(writer.durable_transcript_enabled());
    }

    #[tokio::test]
    async fn disk_full_recovery_never_flushes_another_sessions_pending_rows() {
        let (_dir, orch, _writer, path) = fixture();
        let pending = row(&orch, "original session only").await;
        orch.retain_storage_row(&pending).await;
        let original = orch.session.lock().await.session_id;
        orch.session.lock().await.session_id = SessionId::new();
        orch.recover_session_storage().await.unwrap();
        assert!(!path.exists());
        orch.session.lock().await.session_id = original;
        assert!(orch.check_storage_pause().await.is_err());
        orch.recover_session_storage().await.unwrap();
        assert!(std::fs::read_to_string(path)
            .unwrap()
            .contains("original session only"));
    }
}
