//! Crash-safe durable metadata for ordinary mobile conversation turns.
//!
//! This store deliberately does not claim arbitrary tool replay is exactly
//! once. It persists the user input, ordered client events, and a conservative
//! recovery gate. A caller may resume automatically only while
//! `safe_to_resume` is true; permission-bound or side-effectful work must park
//! as `WaitingForUser`.

use client::protocol::commands::{ImageRefDto, PromptModeDto};
use client::protocol::events::{TurnRecoverySnapshotDto, TurnRecoveryStateDto};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DurableTurnEvent {
    pub(crate) sequence: u64,
    pub(crate) event_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableTurnCheckpoint {
    pub(crate) session_id: String,
    pub(crate) turn_id: u64,
    pub(crate) prompt: String,
    pub(crate) prompt_mode: Option<PromptModeDto>,
    pub(crate) images: Vec<ImageRefDto>,
    pub(crate) revision: u64,
    pub(crate) state: TurnRecoveryStateDto,
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
    #[serde(default)]
    prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_mode: Option<PromptModeDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageRefDto>,
    #[serde(default)]
    revision: u64,
    state: TurnRecoveryStateDto,
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
    pub(crate) fn snapshot(&self) -> TurnRecoverySnapshotDto {
        TurnRecoverySnapshotDto {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id,
            state: self.state.clone(),
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
        self.safe_to_resume
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
            prompt_mode: self.prompt_mode,
            images: self.images.clone(),
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
            prompt_mode: metadata.prompt_mode,
            images: metadata.images,
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
        if checkpoint.state != TurnRecoveryStateDto::Cancelled {
            if let Some((state, reason)) = checkpoint
                .events
                .iter()
                .rev()
                .find_map(|event| terminal_state_from_event(&event.event_json))
            {
                checkpoint.state = state;
                checkpoint.safe_to_resume = false;
                checkpoint.reason = reason;
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

        if self.state.is_terminal() {
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
    #[error("durable turn storage failure: {0}")]
    Storage(String),
}

/// One process owns this store at a time. Metadata stays atomic while retained
/// events live in a crash-tolerant append journal with bounded compaction.
pub(crate) struct DurableTurnStore {
    root: PathBuf,
    lock: Mutex<HashMap<PathBuf, CachedTurn>>,
    #[cfg(test)]
    journal_sync_count: AtomicUsize,
}

impl DurableTurnStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(HashMap::new()),
            #[cfg(test)]
            journal_sync_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn begin(
        &self,
        session_id: &str,
        turn_id: u64,
        prompt: String,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
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
            prompt_mode,
            images,
            revision: 1,
            state: TurnRecoveryStateDto::Running,
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
        Ok(checkpoint)
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
            if cached.checkpoint.state.is_terminal()
                && !(cached.checkpoint.state == TurnRecoveryStateDto::Cancelled && terminal_event)
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
            if checkpoint.state.is_terminal() && checkpoint.state != state {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = state;
            checkpoint.safe_to_resume = safe_to_resume && !checkpoint.state.is_terminal();
            checkpoint.reason = reason;
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
                checkpoint.state,
                TurnRecoveryStateDto::Running | TurnRecoveryStateDto::WaitingForUser
            ) {
                return Ok(None);
            }
            checkpoint.state = state;
            checkpoint.safe_to_resume = safe_to_resume && !checkpoint.state.is_terminal();
            checkpoint.reason = reason;
            Ok(Some(checkpoint.snapshot()))
        })
    }

    pub(crate) fn cancel(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<TurnRecoverySnapshotDto, DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            if checkpoint.state.is_terminal() {
                return Err(DurableTurnStoreError::Terminal {
                    session_id: session_id.to_string(),
                    turn_id,
                });
            }
            checkpoint.state = TurnRecoveryStateDto::Cancelled;
            checkpoint.safe_to_resume = false;
            checkpoint.reason = Some("explicit_cancel".to_string());
            Ok(checkpoint.snapshot())
        })
    }

    pub(crate) fn resume(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(ResumeDisposition, TurnRecoverySnapshotDto), DurableTurnStoreError> {
        self.update(session_id, turn_id, |checkpoint| {
            let disposition = if checkpoint.state.is_terminal() {
                ResumeDisposition::Terminal
            } else if checkpoint.can_resume_without_user() {
                checkpoint.state = TurnRecoveryStateDto::Running;
                checkpoint.reason = None;
                ResumeDisposition::Ready
            } else {
                checkpoint.state = TurnRecoveryStateDto::WaitingForUser;
                checkpoint.safe_to_resume = false;
                if checkpoint.reason.is_none() {
                    checkpoint.reason = Some("replay_boundary_requires_user".to_string());
                }
                ResumeDisposition::WaitingForUser
            };
            Ok((disposition, checkpoint.snapshot()))
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

        let cached = self.load_cached_turn(&mut cache, &relative)?.clone();
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
    fn recovery_merges_legacy_metadata_prefix_with_journal_suffix() {
        let metadata = StoredTurnMetadata {
            session_id: "session-a".to_string(),
            turn_id: 6,
            prompt: "hello".to_string(),
            prompt_mode: None,
            images: Vec::new(),
            revision: 2,
            state: TurnRecoveryStateDto::Running,
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
            .begin("session-a", 7, "hello".to_string(), None, Vec::new())
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
            .begin("session-a", 71, "hello".to_string(), None, Vec::new())
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
            .begin("session-a", 711, "hello".to_string(), None, Vec::new())
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
            .begin("session-a", 72, "hello".to_string(), None, Vec::new())
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
        assert_eq!(recovered.state, TurnRecoveryStateDto::Completed);
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
            .begin("session-a", 721, "hello".to_string(), None, Vec::new())
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
        assert_eq!(recovered.state, TurnRecoveryStateDto::Failed);
        assert_eq!(recovered.reason.as_deref(), Some("provider failed"));
        assert!(!recovered.safe_to_resume);
    }

    #[test]
    fn explicit_cancel_is_terminal_and_cannot_resume_or_append() {
        let (_temp, store) = store();
        store
            .begin("session-a", 8, "hello".to_string(), None, Vec::new())
            .expect("begin");
        let cancelled = store.cancel("session-a", 8).expect("cancel");
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
            .begin("session-a", 9, "deploy".to_string(), None, Vec::new())
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
        assert_eq!(snapshot.state, TurnRecoveryStateDto::WaitingForUser);
        assert!(!snapshot.safe_to_resume);
    }

    #[test]
    fn safe_checkpoint_resumes_running() {
        let (_temp, store) = store();
        store
            .begin("session-a", 10, "read".to_string(), None, Vec::new())
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
        assert_eq!(snapshot.state, TurnRecoveryStateDto::Running);
        assert!(snapshot.safe_to_resume);
        assert!(snapshot.reason.is_none());
    }

    #[test]
    fn replayed_output_requires_user_instead_of_rerunning_the_prompt() {
        let (_temp, store) = store();
        store
            .begin("session-a", 11, "write".to_string(), None, Vec::new())
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
        assert_eq!(snapshot.state, TurnRecoveryStateDto::WaitingForUser);
        assert!(!snapshot.safe_to_resume);
    }

    #[test]
    fn turn_started_checkpoint_still_resumes_running() {
        let (_temp, store) = store();
        store
            .begin("session-a", 12, "read".to_string(), None, Vec::new())
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
            prompt: "gap".to_string(),
            prompt_mode: None,
            images: Vec::new(),
            revision: 1,
            state: TurnRecoveryStateDto::Running,
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
            .begin("session-a", 14, "hello".to_string(), None, Vec::new())
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
            .begin("session-a", 141, "hello".to_string(), None, Vec::new())
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
            .begin("session-a", 15, "hello".to_string(), None, Vec::new())
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
