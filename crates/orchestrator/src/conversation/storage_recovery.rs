use super::*;
use session::JsonlMessage;
use std::future::Future;

#[derive(Clone)]
pub(crate) struct DeferredStorageNotification {
    session: String,
    paused: bool,
}

tokio::task_local! {
    static DEFERRED_STORAGE_NOTIFICATIONS:
        Arc<std::sync::Mutex<Vec<DeferredStorageNotification>>>;
}

const STORAGE_RECOVERED: &str = "Disk space is available again. Pending records were saved; continuing this session with its existing history.";

const DISK_FULL_PAUSE: &str = "Session paused because disk space is insufficient (ENOSPC). Free disk space, then retry in this same session. Existing history is retained; pending records will be saved before continuing.";

#[derive(Default)]
pub(crate) struct StoragePause {
    rows: std::collections::VecDeque<JsonlMessage>,
}

impl ConversationOrchestrator {
    /// Retain storage state immediately, but publish callbacks after the
    /// caller's state transaction has released its locks. Nested owners share
    /// the outer collector and leave publication to that outermost owner.
    pub(crate) async fn with_deferred_storage_notifications<F: Future>(
        &self,
        future: F,
    ) -> (F::Output, Vec<DeferredStorageNotification>) {
        if DEFERRED_STORAGE_NOTIFICATIONS.try_with(|_| ()).is_ok() {
            return (future.await, Vec::new());
        }
        let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let value = DEFERRED_STORAGE_NOTIFICATIONS
            .scope(notices.clone(), future)
            .await;
        let notices = std::mem::take(
            &mut *notices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        (value, notices)
    }

    pub(crate) async fn emit_deferred_storage_notifications(
        &self,
        notices: Vec<DeferredStorageNotification>,
    ) {
        for notice in notices {
            let active =
                self.session.lock().await.session_id.as_uuid().to_string() == notice.session;
            let paused = self
                .transcript
                .storage_pauses
                .lock()
                .await
                .contains_key(&notice.session);
            if active && paused == notice.paused {
                self.output.emit_session_storage_state(notice.paused).await;
                // The first callback may itself switch/recover the session.
                let active =
                    self.session.lock().await.session_id.as_uuid().to_string() == notice.session;
                let paused = self
                    .transcript
                    .storage_pauses
                    .lock()
                    .await
                    .contains_key(&notice.session);
                if active && paused == notice.paused {
                    let message = if notice.paused {
                        DISK_FULL_PAUSE
                    } else {
                        STORAGE_RECOVERED
                    };
                    self.output
                        .emit_system_notice(&message.into(), notice.paused)
                        .await;
                }
            }
        }
    }

    pub(crate) async fn pause_for_disk_full(&self) {
        let session = self.session.lock().await.session_id.as_uuid().to_string();
        self.mark_storage_paused(session).await;
    }

    pub(super) async fn mark_storage_paused(&self, session: String) {
        let active = self.session.lock().await.session_id.as_uuid().to_string() == session;
        let newly_paused = {
            let mut pauses = self.transcript.storage_pauses.lock().await;
            if let std::collections::hash_map::Entry::Vacant(entry) = pauses.entry(session.clone())
            {
                entry.insert(StoragePause::default());
                true
            } else {
                false
            }
        };
        if newly_paused && active {
            self.defer_storage_notification(DeferredStorageNotification {
                session,
                paused: true,
            })
            .await;
        }
    }

    async fn defer_storage_notification(&self, notice: DeferredStorageNotification) {
        if DEFERRED_STORAGE_NOTIFICATIONS
            .try_with(|notices| {
                notices
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(notice.clone());
            })
            .is_err()
        {
            self.emit_deferred_storage_notifications(vec![notice]).await;
        }
    }

    /// Retained rows and the logical cursor become visible together. Any
    /// storage callback belongs to the outer transaction's notification scope.
    async fn retain_storage_row(&self, row: &JsonlMessage) {
        let ((), notices) = self
            .with_deferred_storage_notifications(async {
                self.mark_storage_paused(row.session_id.clone()).await;
                let mut pauses = self.transcript.storage_pauses.lock().await;
                let pause = pauses.entry(row.session_id.clone()).or_default();
                if !pause.rows.iter().any(|pending| pending.uuid == row.uuid) {
                    pause.rows.push_back(row.clone());
                    *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
                }
            })
            .await;
        self.emit_deferred_storage_notifications(notices).await;
    }

    /// Append a linear row against the cursor selected by this transaction,
    /// never against a cursor sampled before an external callback or await.
    pub(crate) async fn append_transcript_row(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
    ) -> Result<(), session::jsonl::writer::WriterError> {
        self.append_transcript_row_with_parent(writer, row, false)
            .await
            .map(|_| ())
    }

    /// Return the actual parent for streaming tombstones. Explicit tool-result
    /// parents and compact roots keep their supplied parent (including None).
    pub(crate) async fn append_transcript_row_with_parent(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
        explicit_parent: bool,
    ) -> Result<Option<String>, session::jsonl::writer::WriterError> {
        let (result, notices) = self
            .with_deferred_storage_notifications(async {
                let _append = self.transcript.storage_append_lock.lock().await;
                let mut row = row.clone();
                if !explicit_parent {
                    row.parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
                }
                let parent = row.parent_uuid.clone();
                self.append_transcript_row_locked(writer, &row).await?;
                Ok(parent)
            })
            .await;
        self.emit_deferred_storage_notifications(notices).await;
        result
    }

    async fn append_transcript_row_locked(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
    ) -> Result<(), session::jsonl::writer::WriterError> {
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
        if result.is_ok() {
            *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
        } else if result
            .as_ref()
            .is_err_and(|error| cost::persistence::is_disk_full(&error.to_string()))
        {
            self.retain_storage_row(row).await;
        }
        result
    }

    async fn append_transcript_batch_locked(
        &self,
        writer: &session::jsonl::JsonlWriter,
        rows: &[JsonlMessage],
    ) -> Result<(), (usize, session::jsonl::writer::WriterError)> {
        if rows.is_empty() {
            return Ok(());
        }
        let pending = self
            .transcript
            .storage_pauses
            .lock()
            .await
            .get(&rows[0].session_id)
            .is_some_and(|pause| !pause.rows.is_empty());
        if pending {
            for row in rows {
                self.retain_storage_row(row).await;
            }
            return Err((
                0,
                lingxi_core::host::FsError::Io(DISK_FULL_PAUSE.into()).into(),
            ));
        }
        let result = writer.append_batch(rows).await;
        if let Err((prefix, error)) = &result {
            if cost::persistence::is_disk_full(&error.to_string()) {
                for row in &rows[*prefix..] {
                    self.retain_storage_row(row).await;
                }
            }
        }
        result
    }

    /// Parent selection, acknowledged-prefix settlement, ENOSPC retention and
    /// fallback reparents all share the ordinary single-row append transaction.
    pub(crate) async fn append_prepared_transcript_rows(
        &self,
        writer: &session::jsonl::JsonlWriter,
        rows: &mut [JsonlMessage],
    ) -> Vec<Result<(), session::jsonl::writer::WriterError>> {
        let (outcomes, notices) = self
            .with_deferred_storage_notifications(async {
                let _append = self.transcript.storage_append_lock.lock().await;
                let mut parent = self.transcript.last_jsonl_uuid.lock().await.clone();
                for row in rows.iter_mut() {
                    row.parent_uuid = parent;
                    parent = Some(row.uuid.clone());
                }
                let result = self.append_transcript_batch_locked(writer, rows).await;
                self.settle_prepared_transcript_rows_locked(writer, rows, result)
                    .await
            })
            .await;
        self.emit_deferred_storage_notifications(notices).await;
        outcomes
    }

    #[cfg(test)]
    async fn settle_prepared_transcript_rows(
        &self,
        writer: &session::jsonl::JsonlWriter,
        rows: &mut [JsonlMessage],
        result: Result<(), (usize, session::jsonl::writer::WriterError)>,
    ) -> Vec<Result<(), session::jsonl::writer::WriterError>> {
        let (outcomes, notices) = self
            .with_deferred_storage_notifications(async {
                let _append = self.transcript.storage_append_lock.lock().await;
                self.settle_prepared_transcript_rows_locked(writer, rows, result)
                    .await
            })
            .await;
        self.emit_deferred_storage_notifications(notices).await;
        outcomes
    }

    async fn settle_prepared_transcript_rows_locked(
        &self,
        writer: &session::jsonl::JsonlWriter,
        rows: &mut [JsonlMessage],
        result: Result<(), (usize, session::jsonl::writer::WriterError)>,
    ) -> Vec<Result<(), session::jsonl::writer::WriterError>> {
        let (prefix, failed) = match result {
            Ok(()) => (rows.len(), None),
            Err((prefix, error)) => (prefix, Some(error)),
        };
        let mut outcomes = (0..prefix).map(|_| Ok(())).collect::<Vec<_>>();
        let disk_full = failed
            .as_ref()
            .is_some_and(|error| cost::persistence::is_disk_full(&error.to_string()));
        if prefix > 0 && (!disk_full || prefix == rows.len()) {
            *self.transcript.last_jsonl_uuid.lock().await = Some(rows[prefix - 1].uuid.clone());
        }
        if let Some(error) = failed {
            if prefix < rows.len() {
                let failure = error.to_string();
                outcomes.push(Err(error));
                for row in rows.iter_mut().skip(prefix + 1) {
                    if disk_full {
                        outcomes.push(Err(lingxi_core::host::FsError::Io(failure.clone()).into()));
                        continue;
                    }
                    row.parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
                    outcomes.push(self.append_transcript_row_locked(writer, row).await);
                }
            } else if let Some(row) = rows.last() {
                self.record_transcript_append_failure(&row.session_id, "batch_maintenance", &error)
                    .await;
            }
        }
        outcomes
    }

    pub(crate) async fn append_pending_transcript_row(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
    ) -> Result<
        (session::jsonl::TranscriptAppendOutcome, Option<String>),
        session::jsonl::TranscriptWriterError,
    > {
        let (result, notices) = self
            .with_deferred_storage_notifications(async {
                let _append = self.transcript.storage_append_lock.lock().await;
                let mut retained = row.clone();
                retained.parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
                let pending = self
                    .transcript
                    .storage_pauses
                    .lock()
                    .await
                    .get(&row.session_id)
                    .is_some_and(|pause| !pause.rows.is_empty());
                if pending {
                    self.retain_storage_row(&retained).await;
                    return Err(lingxi_core::host::FsError::Io(DISK_FULL_PAUSE.into()).into());
                }
                // A pending retry derives its parent under the durable writer's
                // identity transaction and returns that transaction's actual tip.
                let result = writer.append_pending_prompt_message_once(row).await;
                if let Ok((_, tip)) = &result {
                    *self.transcript.last_jsonl_uuid.lock().await = tip.clone();
                } else if result
                    .as_ref()
                    .is_err_and(|error| cost::persistence::is_disk_full(&error.to_string()))
                {
                    self.retain_storage_row(&retained).await;
                }
                result
            })
            .await;
        self.emit_deferred_storage_notifications(notices).await;
        result
    }

    /// Compact markers deliberately start a new physical chain. Their
    /// logical-parent fallback and resulting cursor still share the append
    /// transaction with ordinary rows and retained ENOSPC records.
    pub(crate) async fn append_compact_boundary_row(
        &self,
        writer: &session::jsonl::JsonlWriter,
        row: &JsonlMessage,
        exact_native: bool,
    ) -> Result<(), String> {
        let (result, notices) = self
            .with_deferred_storage_notifications(async {
                let _append = self.transcript.storage_append_lock.lock().await;
                let mut row = row.clone();
                if row.logical_parent_uuid.is_none() {
                    row.logical_parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
                }
                if exact_native {
                    match writer.append_native_message_once(&row).await {
                        Ok((_, true)) => {
                            *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
                            Ok(())
                        }
                        Ok((_, false)) => {
                            Err("native compact boundary is not the transcript tip".to_string())
                        }
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    self.append_transcript_row_locked(writer, &row)
                        .await
                        .map_err(|error| error.to_string())
                }
            })
            .await;
        self.emit_deferred_storage_notifications(notices).await;
        result
    }

    /// A compact suffix can intentionally resume an older kept tail. Do not
    /// rewind past a newer lifecycle attachment that committed in the meantime.
    pub(crate) async fn relink_transcript_tip(&self, expected: &str, next: String) {
        let _append = self.transcript.storage_append_lock.lock().await;
        let mut tip = self.transcript.last_jsonl_uuid.lock().await;
        if tip.as_deref() == Some(expected) {
            *tip = Some(next);
        }
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
                writer
                    .append_native_message_once(row)
                    .await
                    .map_err(|error| {
                        OrchestratorError::Internal(format!(
                            "{DISK_FULL_PAUSE} History could not be saved: {error}"
                        ))
                    })?;
                // Retention already advanced the accepted logical cursor.
                // Compaction can then relink it to a kept suffix, whose last
                // row precedes the summary physically. Flushing must preserve
                // that cursor, including when a later retry append fails.
                pause.rows.pop_front();
            }
        }
        pauses.remove(&session);
        drop(pauses);
        drop(_append);
        self.defer_storage_notification(DeferredStorageNotification {
            session,
            paused: false,
        })
        .await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "storage_notification_tests.rs"]
mod notification_tests;

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

    #[cfg(unix)]
    #[tokio::test]
    async fn uncertain_retry_and_fallback_rewrites_fence_the_next_turn() {
        use lingxi_core::host::session_context::ConversationMode;
        use std::os::unix::fs::PermissionsExt;
        for tombstone in [false, true] {
            let (_dir, orch, writer, path) = fixture();
            let sid = orch.session.lock().await.session_id.as_uuid().to_string();
            writer
                .initialize_fixed_session_mode(&sid, ConversationMode::Code, 0)
                .await
                .unwrap();
            let saved = row(&orch, "question").await;
            writer.append(&saved).await.unwrap();
            let assistant_id = MessageId::new();
            let assistant = JsonlMessage::from_projection(&Utf16JsonProjection::parse(&serde_json::json!({
            "type":"assistant", "uuid":assistant_id.as_uuid().to_string(),
            "parentUuid":saved.uuid.clone(), "sessionId":sid, "isSidechain":false,
            "timestamp":"2026-10-23T04:11:27.000Z", "cwd":"/workspace", "version":"test",
            "message":{"id":"retry-attempt", "role":"assistant", "content":"rejected"}
        }).to_string()).unwrap()).unwrap();
            writer.append(&assistant).await.unwrap();
            *orch.transcript.last_jsonl_uuid.lock().await = Some(assistant.uuid.clone());
            let mut name = path.file_name().unwrap().to_os_string();
            name.push(branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX);
            let sidecar = path.parent().unwrap().join(name);
            std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o400)).unwrap();
            if std::fs::OpenOptions::new()
                .write(true)
                .open(&sidecar)
                .is_ok()
            {
                // Privileged test runners can bypass file-mode failure injection.
                return;
            }
            let result = if tombstone {
                orch.remove_assistant_stream_rows(&[
                    crate::streaming_loop::PersistedAssistantRowLink {
                        uuid: assistant.uuid.clone(),
                        parent_uuid: Some(saved.uuid.clone()),
                    },
                ])
                .await
            } else {
                orch.discard_retry_attempt(assistant_id, &[]).await
            };
            let error = result.unwrap_err();
            assert!(error.to_string().contains("rewrite is uncertain"));
            let rewritten = std::fs::read(&path).unwrap();
            assert!(!String::from_utf8_lossy(&rewritten).contains("rejected"));
            let error = orch.run_turn("must not dispatch").await.unwrap_err();
            assert!(error.to_string().contains("rewrite is uncertain"));
            assert_eq!(std::fs::read(&path).unwrap(), rewritten);
        }
    }

    #[tokio::test]
    async fn partial_batch_repairs_non_disk_failure_chain_and_keeps_disk_full_suffix_chain() {
        for disk_full in [false, true] {
            let (_dir, orch, writer, path) = fixture();
            let first = row(&orch, "first acknowledged").await;
            let mut second = row(&orch, "second failed").await;
            second.parent_uuid = Some(first.uuid.clone());
            let mut third = row(&orch, "third pending").await;
            third.parent_uuid = Some(second.uuid.clone());
            let mut rows = [first.clone(), second.clone(), third.clone()];
            writer.append_batch(&rows[..1]).await.unwrap();
            if disk_full {
                // Model the exact suffix retained by append_transcript_batch
                // when the durable writer reports a nonzero prefix.
                orch.retain_storage_row(&second).await;
                orch.retain_storage_row(&third).await;
            }
            let error = lingxi_core::host::FsError::Io(if disk_full {
                "ENOSPC after acknowledged prefix".into()
            } else {
                "permission denied after acknowledged prefix".into()
            });
            let outcomes = orch
                .settle_prepared_transcript_rows(&writer, &mut rows, Err((1, error.into())))
                .await;
            assert!(outcomes[0].is_ok());
            assert!(outcomes[1].is_err());
            let loaded =
                session::jsonl::reader::route_lines(&std::fs::read_to_string(&path).unwrap());
            if disk_full {
                assert!(outcomes[2].is_err());
                assert_eq!(loaded.by_uuid.len(), 1);
                assert_eq!(
                    *orch.transcript.last_jsonl_uuid.lock().await,
                    Some(third.uuid.clone())
                );
                orch.recover_session_storage().await.unwrap();
                let loaded =
                    session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
                assert_eq!(
                    loaded.by_uuid[&third.uuid].parent_uuid.as_ref(),
                    Some(&second.uuid)
                );
            } else {
                assert!(outcomes[2].is_ok());
                assert_eq!(loaded.by_uuid.len(), 2);
                assert_eq!(
                    loaded.by_uuid[&third.uuid].parent_uuid.as_ref(),
                    Some(&first.uuid)
                );
                let snapshot = writer
                    .read_session_message_identity_snapshot(&path)
                    .await
                    .unwrap();
                assert_eq!(snapshot.by_uuid[&first.uuid], 0);
            }
        }
    }

    #[tokio::test]
    async fn acknowledged_batch_keeps_tip_and_reports_postappend_maintenance_failure() {
        for disk_full in [false, true] {
            let (_dir, orch, writer, _path) = fixture();
            let first = row(&orch, "acknowledged first").await;
            let mut second = row(&orch, "acknowledged second").await;
            second.parent_uuid = Some(first.uuid.clone());
            let mut rows = [first, second.clone()];
            writer.append_batch(&rows).await.unwrap();
            let error = session::jsonl::writer::WriterError::Durable(
                session::jsonl::TranscriptWriterError::WrittenButNotDurable(
                    lingxi_core::host::FsError::Io(if disk_full {
                        "ENOSPC: uncertain maintenance rewrite".into()
                    } else {
                        "uncertain maintenance rewrite".into()
                    }),
                ),
            );
            let result = Err((rows.len(), error));
            let outcomes = orch
                .settle_prepared_transcript_rows(&writer, &mut rows, result)
                .await;
            assert!(outcomes.iter().all(Result::is_ok));
            assert_eq!(
                *orch.transcript.last_jsonl_uuid.lock().await,
                Some(second.uuid)
            );
            if disk_full {
                assert!(orch.check_storage_pause().await.is_err());
                assert!(
                    orch.transcript
                        .storage_pauses
                        .lock()
                        .await
                        .values()
                        .all(|pause| pause.rows.is_empty()),
                    "acknowledged rows are never queued again"
                );
            }
        }
    }

    #[tokio::test]
    async fn paused_batch_keeps_original_chain_and_logical_tip_until_recovery() {
        let (_dir, orch, writer, path) = fixture();
        let first = row(&orch, "pending first").await;
        orch.retain_storage_row(&first).await;
        let second = row(&orch, "pending second").await;
        let mut third = row(&orch, "pending third").await;
        third.parent_uuid = Some(second.uuid.clone());
        let mut rows = [second.clone(), third.clone()];
        let outcomes = orch
            .append_prepared_transcript_rows(&writer, &mut rows)
            .await;
        assert!(outcomes.iter().all(Result::is_err));
        assert_eq!(
            *orch.transcript.last_jsonl_uuid.lock().await,
            Some(third.uuid.clone())
        );
        let sid = orch.session.lock().await.session_id.as_uuid().to_string();
        let pauses = orch.transcript.storage_pauses.lock().await;
        let retained = &pauses[&sid].rows;
        assert_eq!(retained.len(), 3);
        assert_eq!(retained[1].parent_uuid.as_ref(), Some(&first.uuid));
        assert_eq!(retained[2].parent_uuid.as_ref(), Some(&second.uuid));
        drop(pauses);
        orch.recover_session_storage().await.unwrap();
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        assert_eq!(loaded.by_uuid.len(), 3);
        assert_eq!(
            loaded.by_uuid[&third.uuid].parent_uuid.as_ref(),
            Some(&second.uuid)
        );
    }

    #[tokio::test]
    async fn hook_free_assistant_batch_preserves_block_parents_and_tool_origins() {
        use lingxi_core::types::{ContentBlock, ToolUseId};
        let (_dir, orch, writer, path) = fixture();
        let saved = row(&orch, "saved").await;
        writer.append(&saved).await.unwrap();
        *orch.transcript.last_jsonl_uuid.lock().await = Some(saved.uuid.clone());
        let tool_id = ToolUseId::from("local-app-batch");
        let assistant = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "before".into(),
                    citations: None,
                },
                ContentBlock::ToolUse {
                    id: tool_id.clone(),
                    name: "LocalApp".into(),
                    input: Utf16JsonProjection::parse("{}").unwrap(),
                    provider_id: None,
                },
                ContentBlock::Text {
                    text: "after".into(),
                    citations: None,
                },
            ],
            stop_reason: None,
            resumable: None,
            per_turn_effort: None,
        };
        orch.session.lock().await.history.push(assistant.clone());
        let origins = orch
            .persist_assistant_per_block(&assistant, None, None, None)
            .await;
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        let rows = loaded
            .messages_in_order
            .iter()
            .filter(|row| row.message_type == "assistant")
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].parent_uuid.as_ref(), Some(&saved.uuid));
        assert_eq!(rows[1].parent_uuid.as_ref(), Some(&rows[0].uuid));
        assert_eq!(rows[2].parent_uuid.as_ref(), Some(&rows[1].uuid));
        assert_eq!(origins[&tool_id], rows[1].uuid);
        assert_eq!(
            orch.source_tool_assistant_uuid(&tool_id).await.as_ref(),
            Some(&rows[1].uuid)
        );
        assert_eq!(
            *orch.transcript.last_jsonl_uuid.lock().await,
            Some(rows[2].uuid.clone())
        );
        let assistant_id = assistant.id().as_uuid().to_string();
        assert_eq!(
            orch.transcript
                .tool_persisted_origins
                .lock()
                .await
                .get(&(tool_id.to_string(), assistant_id))
                .unwrap()
                .as_ref()
                .unwrap(),
            &rows[1].uuid
        );
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

    #[tokio::test]
    async fn concurrent_ordinary_and_lifecycle_appends_choose_parent_inside_one_transaction() {
        let (_dir, orch, _writer, path) = fixture();
        let ordinary = ConversationMessage::user(MessageId::new(), "accepted ordinary row".into());
        let hook_id = MessageId::new();
        let payload = Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","content":"hook context"}"#,
        )
        .unwrap();
        // Both writers reach the shared append boundary before either can
        // append. A parent sampled outside that boundary is stale for writer 2.
        let append = orch.transcript.storage_append_lock.lock().await;
        let ordinary_write = orch.persist_message_to_jsonl(&ordinary);
        let hook_write = orch.persist_hook_attachment_to_jsonl_with_id(hook_id, payload);
        tokio::pin!(ordinary_write, hook_write);
        assert!(futures::poll!(ordinary_write.as_mut()).is_pending());
        assert!(futures::poll!(hook_write.as_mut()).is_pending());
        drop(append);
        ordinary_write.await;
        hook_write.await;
        let following =
            ConversationMessage::user(MessageId::new(), "following accepted row".into());
        orch.persist_message_to_jsonl(&following).await;
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        let ordinary_uuid = ordinary.id().as_uuid().to_string();
        let hook_uuid = hook_id.as_uuid().to_string();
        let following_uuid = following.id().as_uuid().to_string();
        assert_eq!(
            loaded.by_uuid[&hook_uuid].parent_uuid.as_ref(),
            Some(&ordinary_uuid)
        );
        assert_eq!(
            loaded.by_uuid[&following_uuid].parent_uuid.as_ref(),
            Some(&hook_uuid)
        );
        assert_eq!(
            orch.transcript.last_jsonl_uuid.lock().await.as_ref(),
            Some(&following_uuid)
        );
    }

    #[tokio::test]
    async fn explicit_tool_parent_and_compact_root_survive_linear_append_transactions() {
        let (_dir, orch, writer, path) = fixture();
        let first = row(&orch, "first").await;
        orch.append_transcript_row(&writer, &first).await.unwrap();
        let assistant_parent = "explicit-tool-assistant".to_string();
        let mut tool_result = row(&orch, "tool result").await;
        tool_result.parent_uuid = Some(assistant_parent.clone());
        orch.append_transcript_row_with_parent(&writer, &tool_result, true)
            .await
            .unwrap();
        let mut boundary = row(&orch, "boundary").await;
        boundary.parent_uuid = None;
        boundary.logical_parent_uuid = Some(tool_result.uuid.clone());
        orch.append_compact_boundary_row(&writer, &boundary, false)
            .await
            .unwrap();
        let following = row(&orch, "after boundary").await;
        orch.append_transcript_row(&writer, &following)
            .await
            .unwrap();
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        assert_eq!(
            loaded.by_uuid[&tool_result.uuid].parent_uuid.as_ref(),
            Some(&assistant_parent)
        );
        assert_eq!(loaded.by_uuid[&boundary.uuid].parent_uuid, None);
        assert_eq!(
            loaded.by_uuid[&following.uuid].parent_uuid.as_ref(),
            Some(&boundary.uuid)
        );
    }

    #[tokio::test]
    async fn retrying_a_retained_row_does_not_reparent_it_or_rewind_the_logical_tip() {
        let (_dir, orch, writer, path) = fixture();
        let first = row(&orch, "first retained row").await;
        orch.retain_storage_row(&first).await;
        let second = row(&orch, "second retained row").await;
        orch.append_transcript_row(&writer, &second)
            .await
            .unwrap_err();
        orch.append_transcript_row(&writer, &first)
            .await
            .unwrap_err();
        assert_eq!(
            orch.transcript.last_jsonl_uuid.lock().await.as_ref(),
            Some(&second.uuid)
        );
        let session = orch.session.lock().await.session_id.as_uuid().to_string();
        let pending = orch.transcript.storage_pauses.lock().await[&session]
            .rows
            .clone();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].parent_uuid, first.parent_uuid);
        assert_eq!(pending[1].parent_uuid.as_ref(), Some(&first.uuid));
        orch.recover_session_storage().await.unwrap();
        orch.recover_session_storage().await.unwrap();
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        assert_eq!(loaded.by_uuid.len(), 2);
    }
}
