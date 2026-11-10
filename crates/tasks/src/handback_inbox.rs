//! Indexed peer inbox and its snapshot plus append-only recovery journal.
use super::*;
use lingxi_core::host::rooted_fs::{JournalCursor, RootIdentity};
use lingxi_core::host::FileSystem;
use lingxi_core::types::MessageId;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MIN_COMPACTION_BYTES: usize = 64 * 1024;

/// All equality-bearing fields participate in lookup, including both scopes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ReceiptKey {
    run: HandbackRunKey,
    recipient: HandbackRecipient,
    message_id: MessageId,
}

impl From<&HandbackReceipt> for ReceiptKey {
    fn from(receipt: &HandbackReceipt) -> Self {
        Self {
            run: receipt.run,
            recipient: receipt.recipient,
            message_id: receipt.message_id,
        }
    }
}

/// Cloning the handle never copies pending reports or consumed history.
#[derive(Clone)]
pub(crate) struct PeerInbox {
    pub(super) scope: HandbackSessionScope,
    recipient: AgentId,
    root_identity: Option<RootIdentity>,
    state: Arc<Mutex<InboxState>>,
}

#[derive(Default)]
struct InboxState {
    pending: BTreeMap<u64, HandbackEnvelope>,
    pending_index: HashMap<ReceiptKey, u64>,
    next_pending: u64,
    consumed: Vec<HandbackReceipt>,
    consumed_index: HashSet<ReceiptKey>,
    sequence: u64,
    journal_cursor: Option<JournalCursor>,
    journal_bytes: usize,
    compact_after: usize,
    /// A failed append that could not be durably rolled back must never be
    /// followed by another append or by admission to a fallback recipient.
    uncertain: bool,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    version: u32,
    scope: HandbackSessionScope,
    recipient: AgentId,
    #[serde(default)]
    sequence: u64,
    pending: Vec<HandbackEnvelope>,
    consumed: Vec<HandbackReceipt>,
}

#[derive(serde::Serialize)]
struct SnapshotRef<'a> {
    version: u32,
    scope: HandbackSessionScope,
    recipient: AgentId,
    sequence: u64,
    pending: Vec<&'a HandbackEnvelope>,
    consumed: &'a [HandbackReceipt],
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    version: u32,
    sequence: u64,
    operation: Operation,
}

#[derive(serde::Serialize, serde::Deserialize)]
// External tagging lets serde_json deserialize the envelope directly. An
// internal tag buffers fields and loses the raw JSON protocol required by
// the exact UTF-16 body projection, including unpaired source code units.
#[serde(rename_all = "snake_case")]
enum Operation {
    Admit { envelope: HandbackEnvelope },
    Consume { receipt: HandbackReceipt },
}

pub(super) struct WriteError {
    pub(super) error: TaskError,
    pub(super) uncertain: bool,
}

impl From<TaskError> for WriteError {
    fn from(error: TaskError) -> Self {
        Self {
            error,
            uncertain: false,
        }
    }
}

fn invalid(message: impl std::fmt::Display) -> TaskError {
    TaskError::Internal(format!("invalid retained peer inbox: {message}"))
}

fn io(error: lingxi_core::host::FsError) -> TaskError {
    TaskError::Io(error.to_string())
}

fn valid_receipt(
    receipt: &HandbackReceipt,
    scope: HandbackSessionScope,
    recipient: AgentId,
) -> bool {
    receipt.run.scope.session_id == scope.session_id
        && receipt.run.scope == receipt.recipient.scope()
        && matches!(receipt.recipient, HandbackRecipient::Agent { agent_id, .. } if agent_id == recipient)
}

impl Operation {
    fn valid_for(&self, scope: HandbackSessionScope, recipient: AgentId) -> bool {
        match self {
            Self::Admit { envelope } => {
                envelope.validate() && valid_receipt(&envelope.receipt, scope, recipient)
            }
            Self::Consume { receipt } => valid_receipt(receipt, scope, recipient),
        }
    }
}

impl InboxState {
    fn apply(&mut self, operation: Operation) -> Result<(), TaskError> {
        match operation {
            Operation::Admit { envelope } => {
                let key = ReceiptKey::from(&envelope.receipt);
                if self.consumed_index.contains(&key) || self.pending_index.contains_key(&key) {
                    return Ok(());
                }
                let next = self
                    .next_pending
                    .checked_add(1)
                    .ok_or_else(|| invalid("pending order exhausted"))?;
                self.pending_index.insert(key, self.next_pending);
                self.pending.insert(self.next_pending, envelope);
                self.next_pending = next;
            }
            Operation::Consume { receipt } => {
                let key = ReceiptKey::from(&receipt);
                if self.consumed_index.contains(&key) {
                    return Ok(());
                }
                let index = self
                    .pending_index
                    .remove(&key)
                    .ok_or_else(|| invalid("consumption has no matching admitted receipt"))?;
                self.pending.remove(&index);
                self.consumed_index.insert(key);
                self.consumed.push(receipt);
            }
        }
        Ok(())
    }
}

impl PeerInbox {
    pub(super) fn snapshot_path(scope: HandbackSessionScope, recipient: AgentId) -> PathBuf {
        format!(
            "{}-{}.handback-inbox.json",
            scope.session_id.as_uuid().simple(),
            recipient.as_uuid().simple()
        )
        .into()
    }

    fn journal_path(scope: HandbackSessionScope, recipient: AgentId) -> PathBuf {
        Self::snapshot_path(scope, recipient).with_extension("jsonl")
    }

    fn snapshot_json(&self, state: &InboxState) -> Result<String, TaskError> {
        serde_json::to_string(&SnapshotRef {
            version: 2,
            scope: self.scope,
            recipient: self.recipient,
            sequence: state.sequence,
            pending: state.pending.values().collect(),
            consumed: &state.consumed,
        })
        .map_err(|error| TaskError::Internal(error.to_string()))
    }

    pub(super) fn pending_reports(&self) -> Vec<HandbackEnvelope> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .values()
            .cloned()
            .collect()
    }

    pub(super) fn healthy(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .uncertain
    }

    pub(super) async fn create(
        fs: &dyn FileSystem,
        root: &Path,
        root_identity: Option<RootIdentity>,
        scope: HandbackSessionScope,
        recipient: AgentId,
    ) -> Result<Self, TaskError> {
        let journal = fs
            .read_file_rooted_journal_pinned(
                root,
                &Self::journal_path(scope, recipient),
                root_identity.as_ref(),
            )
            .await
            .map_err(io)?;
        if journal
            .bytes
            .as_ref()
            .is_some_and(|bytes| !bytes.is_empty())
        {
            return Err(invalid("cannot create a base over an existing journal"));
        }
        let inbox = Self {
            scope,
            recipient,
            root_identity,
            state: Arc::new(Mutex::new(InboxState {
                journal_cursor: Some(journal.cursor),
                compact_after: MIN_COMPACTION_BYTES,
                ..InboxState::default()
            })),
        };
        let json = inbox.snapshot_json(&InboxState::default())?;
        // Publishing an empty base cannot commit a report, even when an
        // atomic-write error is returned after its rename.
        fs.write_file_rooted_atomic_pinned(
            root,
            &Self::snapshot_path(scope, recipient),
            &json,
            root_identity.as_ref(),
        )
        .await
        .map_err(io)?;
        Ok(inbox)
    }

    pub(super) async fn load(
        fs: &dyn FileSystem,
        root: &Path,
        root_identity: Option<RootIdentity>,
        scope: HandbackSessionScope,
        recipient: AgentId,
    ) -> Result<Option<Self>, TaskError> {
        let snapshot = fs
            .read_file_rooted_journal_pinned(
                root,
                &Self::snapshot_path(scope, recipient),
                root_identity.as_ref(),
            )
            .await
            .map_err(io)?
            .bytes;
        let journal_path = Self::journal_path(scope, recipient);
        let journal = fs
            .read_file_rooted_journal_pinned(root, &journal_path, root_identity.as_ref())
            .await
            .map_err(io)?;
        let mut journal_cursor = journal.cursor;
        let journal = journal.bytes.unwrap_or_default();
        let Some(snapshot) = snapshot.filter(|snapshot| !snapshot.is_empty()) else {
            return if journal.is_empty() {
                Ok(None)
            } else {
                Err(invalid("journal has no snapshot base"))
            };
        };
        let snapshot_bytes = snapshot.len();
        let snapshot: Snapshot = serde_json::from_slice(&snapshot).map_err(invalid)?;
        if !matches!(snapshot.version, 1 | 2)
            || (snapshot.version == 1 && snapshot.sequence != 0)
            || snapshot.scope.session_id != scope.session_id
            || snapshot.recipient != recipient
        {
            return Err(invalid(
                "snapshot has an invalid recipient, version or origin",
            ));
        }
        let mut state = InboxState {
            sequence: snapshot.sequence,
            compact_after: snapshot_bytes.max(MIN_COMPACTION_BYTES),
            ..InboxState::default()
        };
        for receipt in snapshot.consumed {
            if !valid_receipt(&receipt, scope, recipient)
                || !state.consumed_index.insert(ReceiptKey::from(&receipt))
            {
                return Err(invalid("invalid or duplicate consumed receipt"));
            }
            state.consumed.push(receipt);
        }
        for envelope in snapshot.pending {
            let key = ReceiptKey::from(&envelope.receipt);
            if !envelope.validate()
                || !valid_receipt(&envelope.receipt, scope, recipient)
                || state.pending_index.contains_key(&key)
                || state.consumed_index.contains(&key)
            {
                return Err(invalid("invalid or duplicate pending receipt"));
            }
            state.apply(Operation::Admit { envelope })?;
        }
        // A newline belongs to the committed frame. An incomplete final
        // append can never have returned a successful durable receipt.
        let complete_bytes = journal
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let mut previous = None;
        for line in journal[..complete_bytes].split_inclusive(|byte| *byte == b'\n') {
            let record: Record = serde_json::from_slice(line).map_err(invalid)?;
            if record.version != 1
                || record.sequence == 0
                || previous.is_some_and(|prior: u64| prior.checked_add(1) != Some(record.sequence))
                || !record.operation.valid_for(scope, recipient)
            {
                return Err(invalid("invalid journal record or sequence"));
            }
            previous = Some(record.sequence);
            if record.sequence <= snapshot.sequence {
                continue;
            }
            if state.sequence.checked_add(1) != Some(record.sequence) {
                return Err(invalid("journal sequence gap"));
            }
            state.apply(record.operation)?;
            state.sequence = record.sequence;
        }
        if complete_bytes != journal.len() {
            // Validate every complete record before touching a torn suffix.
            journal_cursor = fs
                .truncate_file_rooted_journal_durable(
                    root,
                    &journal_path,
                    &journal_cursor,
                    complete_bytes as u64,
                )
                .await
                .map_err(|error| io(error.error))?;
        } else if !journal.is_empty() {
            // A process may have died after write but before fsync. Replay
            // crosses that boundary before exposing any recovered admission.
            fs.sync_file_rooted_journal_durable(root, &journal_path, &journal_cursor)
                .await
                .map_err(io)?;
        }
        state.journal_bytes = complete_bytes;
        state.journal_cursor = Some(journal_cursor);
        Ok(Some(Self {
            scope: snapshot.scope,
            recipient,
            root_identity,
            state: Arc::new(Mutex::new(state)),
        }))
    }

    pub(super) async fn admit(
        &self,
        fs: &dyn FileSystem,
        root: &Path,
        envelope: HandbackEnvelope,
    ) -> Result<bool, WriteError> {
        let key = ReceiptKey::from(&envelope.receipt);
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.consumed_index.contains(&key) || state.pending_index.contains_key(&key) {
                return Ok(false);
            }
        }
        self.append(fs, root, Operation::Admit { envelope }).await?;
        Ok(true)
    }

    pub(super) async fn consume(
        &self,
        fs: &dyn FileSystem,
        root: &Path,
        receipt: &HandbackReceipt,
    ) -> Result<bool, WriteError> {
        let key = ReceiptKey::from(receipt);
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.consumed_index.contains(&key) {
                return Ok(true);
            }
            if !state.pending_index.contains_key(&key) {
                return Ok(false);
            }
        }
        self.append(
            fs,
            root,
            Operation::Consume {
                receipt: receipt.clone(),
            },
        )
        .await?;
        Ok(true)
    }

    /// The registry's handback transaction serializes these writers. The
    /// synchronous state lock is released throughout every filesystem await.
    async fn append(
        &self,
        fs: &dyn FileSystem,
        root: &Path,
        operation: Operation,
    ) -> Result<(), WriteError> {
        let (sequence, cursor) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.uncertain {
                return Err(WriteError {
                    error: TaskError::Io("peer inbox requires durable recovery".into()),
                    uncertain: true,
                });
            }
            if matches!(&operation, Operation::Admit { .. }) && state.next_pending == u64::MAX {
                return Err(invalid("pending order exhausted").into());
            }
            (
                state
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid("journal sequence exhausted"))?,
                state
                    .journal_cursor
                    .ok_or_else(|| invalid("missing journal cursor"))?,
            )
        };
        let record = Record {
            version: 1,
            sequence,
            operation,
        };
        let mut line = serde_json::to_string(&record).map_err(invalid)?;
        line.push('\n');
        let journal_path = Self::journal_path(self.scope, self.recipient);
        let next_cursor = match fs
            .append_file_rooted_journal_durable(root, &journal_path, &cursor, &line)
            .await
        {
            Ok(cursor) => cursor,
            Err(error) => {
                {
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(cursor) = error.recovered {
                        state.journal_cursor = Some(cursor);
                        state.journal_bytes = usize::try_from(cursor.len()).unwrap_or(usize::MAX);
                    }
                    state.uncertain |= error.uncertain;
                }
                return Err(WriteError {
                    error: io(error.error),
                    uncertain: error.uncertain,
                });
            }
        };
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = state.apply(record.operation) {
                // Even an unexpected in-memory invariant failure cannot
                // turn a committed record into permission to send a fallback.
                state.uncertain = true;
                return Err(WriteError {
                    error,
                    uncertain: true,
                });
            }
            state.sequence = sequence;
            state.journal_cursor = Some(next_cursor);
            state.journal_bytes = usize::try_from(next_cursor.len()).unwrap_or(usize::MAX);
        }
        self.compact(fs, root).await;
        Ok(())
    }

    async fn compact(&self, fs: &dyn FileSystem, root: &Path) {
        let (json, cursor) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.journal_bytes < state.compact_after {
                return;
            }
            let Ok(json) = self.snapshot_json(&state) else {
                return;
            };
            // Snapshot work is charged to at least its size in new journal
            // bytes. Failed maintenance also waits for that much new work.
            state.compact_after = state
                .journal_bytes
                .saturating_add(json.len().max(MIN_COMPACTION_BYTES));
            (json, state.journal_cursor.expect("loaded journal cursor"))
        };
        if fs
            .write_file_rooted_atomic_pinned(
                root,
                &Self::snapshot_path(self.scope, self.recipient),
                &json,
                self.root_identity.as_ref(),
            )
            .await
            .is_err()
        {
            return;
        }
        // The snapshot watermark is published first. If a crash leaves the
        // old journal, recovery validates and skips its covered records.
        let path = Self::journal_path(self.scope, self.recipient);
        let cleared = fs
            .truncate_file_rooted_journal_durable(root, &path, &cursor, 0)
            .await;
        let recovered = match &cleared {
            Err(error) if error.uncertain => {
                // Maintenance already published a complete snapshot. On a
                // sync error, replay can prove whether this exact journal
                // inode still has a usable old prefix or was cleared.
                match Self::load(fs, root, self.root_identity, self.scope, self.recipient).await {
                    Ok(Some(inbox)) => {
                        let state = inbox
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state
                            .journal_cursor
                            .filter(|recovered| recovered.file == cursor.file)
                            .map(|recovered| (state.sequence, recovered))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cleared {
            Ok(cursor) => {
                state.journal_cursor = Some(cursor);
                state.journal_bytes = 0;
                state.compact_after = json.len().max(MIN_COMPACTION_BYTES);
            }
            Err(error) => {
                let recovered = recovered
                    .filter(|(sequence, _)| *sequence == state.sequence)
                    .map(|(_, cursor)| cursor)
                    .or(error.recovered.filter(|_| !error.uncertain));
                if let Some(cursor) = recovered {
                    let bytes = usize::try_from(cursor.len()).unwrap_or(usize::MAX);
                    state.journal_cursor = Some(cursor);
                    state.journal_bytes = bytes;
                    state.compact_after =
                        bytes.saturating_add(json.len().max(MIN_COMPACTION_BYTES));
                } else if error.uncertain {
                    state.uncertain = true;
                }
            }
        }
    }
}
