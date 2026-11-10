//! Durable attempt ownership for ordinary deferred tools, independent of JSONL.

use super::{ConversationOrchestrator, DeferredReplayOutcome};
use crate::OrchestratorError;
use lingxi_core::host::DeferredToolReplay;
use lingxi_core::types::{ContentBlock, SessionId};
use serde::{Deserialize, Serialize};
use session::jsonl::DurableJournal;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn error(message: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::Internal(format!("deferred replay checkpoint: {message}"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplayDescriptor {
    version: u32,
    session_id: SessionId,
    tool_use_id: String,
    tool_name: String,
    input_json: String,
    assistant_message_id: Option<String>,
    permission_mode: Option<String>,
}

impl ReplayDescriptor {
    pub(super) fn new(
        session_id: SessionId,
        tool: &DeferredToolReplay,
    ) -> Result<Self, OrchestratorError> {
        if tool.tool_use_id.is_empty() || tool.tool_name.is_empty() {
            return Err(error("deferred call has no stable identity"));
        }
        Ok(Self {
            version: 1,
            session_id,
            tool_use_id: tool.tool_use_id.clone(),
            tool_name: tool.tool_name.clone(),
            input_json: tool.tool_input.to_json_string().map_err(error)?,
            assistant_message_id: tool.assistant_message_id.map(|id| id.to_string()),
            permission_mode: tool.permission_mode.clone(),
        })
    }

    fn key(&self) -> String {
        let mut hash = Sha256::new();
        for value in [self.session_id.to_string(), self.tool_use_id.clone()] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        format!("deferred:{:x}", hash.finalize())
    }

    fn validate(&self, session: SessionId) -> Result<(), OrchestratorError> {
        if self.version != 1
            || self.session_id != session
            || self.tool_use_id.is_empty()
            || self.tool_name.is_empty()
        {
            return Err(error(
                "checkpoint descriptor does not match its session/schema",
            ));
        }
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&self.input_json)
            .map_err(error)?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
enum Checkpoint {
    Started {
        descriptor: ReplayDescriptor,
    },
    Completed {
        descriptor: ReplayDescriptor,
        outcome_json: String,
        context: ContextDelta,
    },
}

pub(super) struct ReplayCheckpoints {
    journal: DurableJournal,
    session_id: SessionId,
}

impl ReplayCheckpoints {
    pub(super) async fn open(
        orch: &ConversationOrchestrator,
        session_id: SessionId,
    ) -> Result<Self, OrchestratorError> {
        let writer = orch.transcript.jsonl_writer.as_ref().ok_or_else(|| {
            error("a durable transcript authority is required before replaying a side effect")
        })?;
        writer
            .verify_active_replay_source(session_id)
            .await
            .map_err(error)?;
        let authority = writer.active_transcript_authority().ok_or_else(|| {
            error("a physical transcript authority is required before replaying a side effect")
        })?;
        let root = orch
            .config_home
            .clone()
            .or_else(|| authority.path().parent().map(std::path::Path::to_path_buf))
            .ok_or_else(|| error("checkpoint root is unavailable"))?;
        let relative = PathBuf::from("deferred-tool-replay").join(session_id.as_uuid().to_string());
        let journal = tokio::task::spawn_blocking(move || {
            DurableJournal::open_under(&root, &relative)
                .map(|journal| journal.with_max_record_bytes(128 * 1024 * 1024))
        })
        .await
        .map_err(error)?
        .map_err(error)?;
        let result = Self {
            journal,
            session_id,
        };
        result.read().await?;
        Ok(result)
    }

    async fn read(
        &self,
    ) -> Result<BTreeMap<String, (ReplayDescriptor, Option<CompletedReplay>)>, OrchestratorError>
    {
        let journal = self.journal.clone();
        let session = self.session_id;
        tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            // Unlike ordinary derivative ledgers, even a torn final record is
            // an unknown side effect here. Never repair/truncate it on replay.
            journal
                .replay_strict_durable_with(|entry| entries.push(entry))
                .map_err(error)?;
            let mut records: BTreeMap<String, (ReplayDescriptor, Option<CompletedReplay>)> =
                BTreeMap::new();
            for entry in entries {
                let checkpoint: Checkpoint = serde_json::from_value(entry.event).map_err(error)?;
                match checkpoint {
                    Checkpoint::Started { descriptor } => {
                        descriptor.validate(session)?;
                        let key = descriptor.key();
                        if entry.event_id != format!("{key}:started")
                            || records.insert(key, (descriptor, None)).is_some()
                        {
                            return Err(error("invalid or duplicate Started identity"));
                        }
                    }
                    Checkpoint::Completed {
                        descriptor,
                        outcome_json,
                        context,
                    } => {
                        descriptor.validate(session)?;
                        let key = descriptor.key();
                        let Some((started, completed)) = records.get_mut(&key) else {
                            return Err(error("completion has no Started record"));
                        };
                        if started != &descriptor
                            || completed.is_some()
                            || entry.event_id != format!("{key}:completed")
                        {
                            return Err(error("completion descriptor/order mismatch"));
                        }
                        let outcome: DeferredReplayOutcome =
                            serde_json::from_str(&outcome_json).map_err(error)?;
                        validate_outcome(&descriptor, &outcome)?;
                        context.validate()?;
                        *completed = Some(CompletedReplay { outcome, context });
                    }
                }
            }
            Ok(records)
        })
        .await
        .map_err(error)?
    }

    pub(super) async fn completed(
        &self,
        descriptor: &ReplayDescriptor,
    ) -> Result<Option<CompletedReplay>, OrchestratorError> {
        let mut records = self.read().await?;
        let Some((stored, completed)) = records.remove(&descriptor.key()) else {
            return Ok(None);
        };
        if stored != *descriptor {
            return Err(error(
                "the same deferred identity has a different call descriptor",
            ));
        }
        completed.map(Some).ok_or_else(|| {
            error(
                "an earlier replay attempt has an unknown outcome; reconcile it before continuing",
            )
        })
    }

    pub(super) async fn start(
        &self,
        descriptor: &ReplayDescriptor,
    ) -> Result<(), OrchestratorError> {
        self.read().await?;
        let event = Checkpoint::Started {
            descriptor: descriptor.clone(),
        };
        let id = format!("{}:started", descriptor.key());
        let journal = self.journal.clone();
        let ack = tokio::task::spawn_blocking(move || journal.append_once(id, &event))
            .await
            .map_err(error)?
            .map_err(error)?;
        if ack.duplicate {
            return Err(error(
                "an earlier replay attempt already owns this side effect",
            ));
        }
        Ok(())
    }

    pub(super) async fn complete(
        &self,
        descriptor: &ReplayDescriptor,
        outcome: &DeferredReplayOutcome,
        context: ContextDelta,
    ) -> Result<(), OrchestratorError> {
        validate_outcome(descriptor, outcome)?;
        self.read().await?;
        let event = Checkpoint::Completed {
            descriptor: descriptor.clone(),
            outcome_json: serde_json::to_string(outcome).map_err(error)?,
            context,
        };
        let id = format!("{}:completed", descriptor.key());
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || journal.append_once(id, &event))
            .await
            .map_err(error)?
            .map_err(error)?;
        Ok(())
    }
}

fn validate_outcome(
    descriptor: &ReplayDescriptor,
    outcome: &DeferredReplayOutcome,
) -> Result<(), OrchestratorError> {
    if outcome.tool_use_id.to_string() != descriptor.tool_use_id || outcome.tool_results.iter().any(|block| !matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &outcome.tool_use_id)) || outcome.injected_messages.iter().any(|(_, id)| id.as_str().is_empty()) {
        return Err(error("completion contains a mismatched tool result"));
    }
    Ok(())
}

pub(super) struct CompletedReplay {
    outcome: DeferredReplayOutcome,
    context: ContextDelta,
}

impl CompletedReplay {
    pub(super) async fn restore(
        self,
        orch: &ConversationOrchestrator,
    ) -> Result<DeferredReplayOutcome, OrchestratorError> {
        if self.context.requires_reconciliation || self.outcome.had_context_modifiers {
            return Err(error("completed tool changed runtime context that requires reconciliation; the tool will not be executed again"));
        }
        self.context.restore(orch).await?;
        Ok(self.outcome)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSnapshot {
    content: String,
    mtime_ms: i64,
    offset: Option<u64>,
    limit: Option<u64>,
    from_read: bool,
    seeded_from_context: bool,
    is_partial_view: bool,
    in_model_context: bool,
    routes: Vec<Vec<PathBuf>>,
}

impl ReadSnapshot {
    fn entry(&self) -> tool_api::read_file_state::ReadFileEntry {
        tool_api::read_file_state::ReadFileEntry {
            content: self.content.clone(),
            mtime_ms: i64::MIN,
            offset: self.offset,
            limit: self.limit,
            from_read: self.from_read,
            seeded_from_context: self.seeded_from_context,
            is_partial_view: self.is_partial_view,
        }
    }
    fn same_content(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.mtime_ms = i64::MIN;
        right.mtime_ms = i64::MIN;
        left == right
    }
}

fn read_snapshot(orch: &ConversationOrchestrator) -> BTreeMap<PathBuf, ReadSnapshot> {
    let cache = orch
        .prompt_runtime
        .read_state_map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let visible = cache.model_context_keys();
    cache
        .keys()
        .into_iter()
        .filter_map(|path| {
            let entry = cache.peek(&path)?;
            let snapshot = ReadSnapshot {
                content: entry.content,
                mtime_ms: entry.mtime_ms,
                offset: entry.offset,
                limit: entry.limit,
                from_read: entry.from_read,
                seeded_from_context: entry.seeded_from_context,
                is_partial_view: entry.is_partial_view,
                in_model_context: visible.contains(&path),
                routes: cache.requested_path_groups(&path),
            };
            Some((path, snapshot))
        })
        .collect()
}

pub(super) struct ReplayContext {
    reads: BTreeMap<PathBuf, ReadSnapshot>,
    todos_json: String,
    other_context: Vec<u8>,
}

impl ReplayContext {
    pub(super) async fn capture(
        orch: &ConversationOrchestrator,
    ) -> Result<Self, OrchestratorError> {
        let mut session = orch.session.lock().await.clone();
        let todos_json = serde_json::to_string(&session.todos).map_err(error)?;
        session.todos.clear();
        session.usage = Default::default();
        session.turns_since_last_todo_write = 0;
        session.turns_since_last_reminder = 0;
        let other_context = serde_json::to_vec(&(
            session,
            orch.current_cwd(),
            orch.session_cwd.snapshot(),
            orch.permission_mode(),
        ))
        .map_err(error)?;
        Ok(Self {
            reads: read_snapshot(orch),
            todos_json,
            other_context,
        })
    }

    pub(super) async fn finish(
        self,
        orch: &ConversationOrchestrator,
        had_modifiers: bool,
    ) -> Result<ContextDelta, OrchestratorError> {
        let after = Self::capture(orch).await?;
        let mut keys = self
            .reads
            .keys()
            .chain(after.reads.keys())
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        let files = keys
            .into_iter()
            .filter_map(|path| {
                let before = self.reads.get(&path).cloned();
                let after = after.reads.get(&path).cloned();
                (before != after).then_some(FileChange {
                    path,
                    before,
                    after,
                })
            })
            .collect();
        let todos = (self.todos_json != after.todos_json).then_some(TodoChange {
            before_json: self.todos_json,
            after_json: after.todos_json,
        });
        Ok(ContextDelta {
            requires_reconciliation: had_modifiers || self.other_context != after.other_context,
            files,
            todos,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChange {
    path: PathBuf,
    before: Option<ReadSnapshot>,
    after: Option<ReadSnapshot>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoChange {
    before_json: String,
    after_json: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextDelta {
    requires_reconciliation: bool,
    files: Vec<FileChange>,
    todos: Option<TodoChange>,
}

impl ContextDelta {
    fn validate(&self) -> Result<(), OrchestratorError> {
        let mut paths = std::collections::HashSet::new();
        for file in &self.files {
            if !file.path.is_absolute() || !paths.insert(&file.path) {
                return Err(error("file context delta has an ambiguous path"));
            }
        }
        if let Some(todos) = &self.todos {
            for source in [&todos.before_json, &todos.after_json] {
                let value: Vec<lingxi_core::TodoItem> =
                    serde_json::from_str(source).map_err(error)?;
                tool_task::todo_write::validate_todos(&value).map_err(error)?;
            }
        }
        Ok(())
    }

    async fn restore(&self, orch: &ConversationOrchestrator) -> Result<(), OrchestratorError> {
        self.validate()?;
        let current = read_snapshot(orch);
        for file in &self.files {
            let same = |expected: &Option<ReadSnapshot>| match (current.get(&file.path), expected) {
                (None, None) => true,
                (Some(current), Some(expected)) => current.same_content(expected),
                _ => false,
            };
            if !same(&file.before) && !same(&file.after) {
                return Err(error(
                    "current file state conflicts with completed replay context",
                ));
            }
        }
        let mut session = orch.session.lock().await;
        if let Some(todos) = &self.todos {
            let current = serde_json::to_string(&session.todos).map_err(error)?;
            if current != todos.before_json && current != todos.after_json {
                return Err(error(
                    "current todo state conflicts with completed replay context",
                ));
            }
        }
        let mut cache = orch
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for file in &self.files {
            cache.remove(&file.path);
            if let Some(after) = &file.after {
                if after.routes.is_empty() {
                    cache.set_with_model_context(
                        file.path.clone(),
                        after.entry(),
                        after.in_model_context,
                    );
                } else {
                    for route in &after.routes {
                        cache.set_with_requested_aliases(
                            file.path.clone(),
                            after.entry(),
                            after.in_model_context,
                            route.clone(),
                        );
                    }
                }
            }
        }
        if let Some(todos) = &self.todos {
            session.todos = serde_json::from_str(&todos.after_json).map_err(error)?;
        }
        Ok(())
    }
}
