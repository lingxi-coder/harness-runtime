//! Session-owned durable transcript targets for the mobile composition root.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use lingxi_core::types::SessionId;
use session::jsonl::DurableTranscriptWriter;

type OwnerKey = (PathBuf, SessionId);

fn owners() -> &'static Mutex<HashMap<OwnerKey, Weak<SessionOwner>>> {
    static OWNERS: OnceLock<Mutex<HashMap<OwnerKey, Weak<SessionOwner>>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn session_owner_is_live(home: PathBuf, session_id: SessionId) -> bool {
    owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(home, session_id))
        .is_some_and(|owner| owner.strong_count() > 0)
}

/// A process-local, lifetime-long admission, distinct from the transaction lock
/// which only serializes individual writes. The canonical home is part of the
/// identity so independent installations may use the same UUID.
struct SessionOwner {
    key: OwnerKey,
}

/// Provisional admission retained while a target transcript is read and
/// replayed. Dropping it before activation releases an otherwise unused claim.
pub(crate) struct MobileSessionReservation {
    _owner: Arc<SessionOwner>,
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        let mut registry = owners()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .get(&self.key)
            .is_some_and(|entry| entry.strong_count() == 0)
        {
            registry.remove(&self.key);
        }
    }
}

/// Prepare transcript authority without changing the active conversation.
pub(crate) struct MobileTranscriptSessionSwitcher {
    home: PathBuf,
    writers: tokio::sync::Mutex<HashMap<SessionId, Arc<DurableTranscriptWriter>>>,
    // Keep old session claims for the whole orchestrator lifetime: background
    // work may still hold an old session's writer after a hot switch.
    claims: Arc<Mutex<OwnerClaims>>,
}

#[derive(Default)]
struct OwnerClaims {
    committed: HashMap<SessionId, Arc<SessionOwner>>,
    pending: HashMap<SessionId, Weak<SessionOwner>>,
}

impl MobileTranscriptSessionSwitcher {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self {
            home,
            writers: Default::default(),
            claims: Default::default(),
        }
    }

    fn claim(&self, session_id: SessionId) -> Result<Arc<SessionOwner>, cost::CostPersistError> {
        let mut claims = self
            .claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = claims.committed.get(&session_id) {
            return Ok(existing.clone());
        }
        claims.pending.retain(|_, owner| owner.strong_count() > 0);
        if let Some(existing) = claims.pending.get(&session_id).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let key = (self.home.clone(), session_id);
        let mut registry = owners()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .get(&key)
            .is_some_and(|entry| entry.strong_count() > 0)
        {
            return Err(cost::CostPersistError::Storage(format!(
                "mobile session {} is already mounted by another live owner",
                session_id.as_uuid()
            )));
        }
        let owner = Arc::new(SessionOwner { key: key.clone() });
        registry.insert(key, Arc::downgrade(&owner));
        claims.pending.insert(session_id, Arc::downgrade(&owner));
        Ok(owner)
    }

    /// Protect a source before reading it. Owned switch preparation reuses
    /// this same claim, never another orchestrator's admission.
    pub(crate) fn reserve(
        &self,
        session_id: SessionId,
    ) -> Result<MobileSessionReservation, cost::CostPersistError> {
        Ok(MobileSessionReservation {
            _owner: self.claim(session_id)?,
        })
    }

    /// A scheduled child may only borrow the exact live owner's admission.
    pub(crate) fn owns(&self, home: &std::path::Path, session_id: SessionId) -> bool {
        self.home == home
            && self
                .claims
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .committed
                .contains_key(&session_id)
    }

    /// Initial assembly claims before creating a writer or launching services.
    pub(crate) fn claim_initial(
        &self,
        session_id: SessionId,
    ) -> Result<(), cost::CostPersistError> {
        let owner = self.claim(session_id)?;
        let mut claims = self
            .claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        claims.pending.remove(&session_id);
        claims.committed.insert(session_id, owner);
        Ok(())
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
        let owner = self.claim(session_id)?;
        let writer = self.writer(session_id).await?;
        let cost = tracker.prepare_session(session_id).await?;
        let claims = self.claims.clone();
        Ok(
            orchestrator::conversation::PreparedSessionSwitch::new(cost, Some(writer))
                .with_on_activated(move || {
                    let mut claims = claims
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    claims.pending.remove(&session_id);
                    claims.committed.insert(session_id, owner);
                }),
        )
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn simultaneous_claims_have_one_winner_and_release_after_drop() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().to_path_buf();
        let id = SessionId::new();
        let start = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for _ in 0..2 {
            let home = home.clone();
            let start = start.clone();
            let release = release.clone();
            let tx = tx.clone();
            threads.push(std::thread::spawn(move || {
                let switcher = MobileTranscriptSessionSwitcher::new(home);
                start.wait();
                let result = switcher.claim_initial(id).is_ok();
                tx.send(result).unwrap();
                release.wait();
            }));
        }
        start.wait();
        let results = [rx.recv().unwrap(), rx.recv().unwrap()];
        assert_eq!(results.into_iter().filter(|value| *value).count(), 1);
        release.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        MobileTranscriptSessionSwitcher::new(home)
            .claim_initial(id)
            .expect("a drained owner must release its identity");
    }

    #[test]
    fn abandoned_prepared_claim_does_not_reserve_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let first = MobileTranscriptSessionSwitcher::new(root.path().to_path_buf());
        let second = MobileTranscriptSessionSwitcher::new(root.path().to_path_buf());
        let id = SessionId::new();
        let pending =
            lingxi_core::host::orchestrator::ResumeLifetimeGuard::new(first.reserve(id).unwrap());
        let detached = pending.clone();
        assert!(second.claim(id).is_err());
        drop(pending);
        assert!(second.claim(id).is_err());
        drop(detached);
        assert!(second.claim(id).is_ok());
    }
}
