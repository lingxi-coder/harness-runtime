//! Session-owned durable transcript targets for the mobile composition root.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::types::SessionId;
use session::jsonl::DurableTranscriptWriter;

/// Prepare transcript authority without changing the active conversation.
pub(crate) struct MobileTranscriptSessionSwitcher {
    home: PathBuf,
    writers: tokio::sync::Mutex<HashMap<SessionId, Arc<DurableTranscriptWriter>>>,
}

impl MobileTranscriptSessionSwitcher {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self {
            home,
            writers: Default::default(),
        }
    }

    pub(crate) async fn writer(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<DurableTranscriptWriter>, cost::CostPersistError> {
        let mut writers = self.writers.lock().await;
        let existing = writers.get(&session_id).cloned();
        let home = self.home.clone();
        let writer = tokio::task::spawn_blocking(move || {
            let writer = if let Some(existing) = existing {
                existing
            } else {
                std::fs::create_dir_all(&home)
                    .map_err(|error| cost::CostPersistError::Storage(error.to_string()))?;
                let relative =
                    PathBuf::from("session-state").join(session_id.as_uuid().to_string());
                let identity = lingxi_core::host::rooted_fs::ensure_private_directory(
                    &home,
                    &relative,
                    session::jsonl::journal::SESSION_STATE_DIR_MODE,
                )
                .map_err(|error| cost::CostPersistError::Storage(error.to_string()))?;
                Arc::new(DurableTranscriptWriter::from_pinned(
                    home.join(relative),
                    identity,
                ))
            };
            // A cached session keeps its original inode authority. Validate it
            // before an owned clear/resume can publish the destination.
            writer
                .with_transaction(|_| Ok(()))
                .map_err(|error| cost::CostPersistError::Storage(error.to_string()))?;
            Ok::<_, cost::CostPersistError>(writer)
        })
        .await
        .map_err(|error| cost::CostPersistError::Storage(error.to_string()))??;
        writers.insert(session_id, writer.clone());
        Ok(writer)
    }
}

#[async_trait]
impl orchestrator::conversation::CostSessionSwitcher for MobileTranscriptSessionSwitcher {
    async fn prepare_session(
        &self,
        tracker: Arc<cost::CostTracker>,
        session_id: SessionId,
    ) -> Result<orchestrator::conversation::PreparedSessionSwitch, cost::CostPersistError> {
        let writer = self.writer(session_id).await?;
        let cost = tracker.prepare_session(session_id).await?;
        Ok(orchestrator::conversation::PreparedSessionSwitch::new(
            cost,
            Some(writer),
        ))
    }
}
