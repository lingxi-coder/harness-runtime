//! Session-scoped subagent observation and transcript helpers for Desktop.
//!
//! The renderer's Agents view is deliberately backed by the same structured
//! observer and append-only JSONL transcripts used by the engine.  Keeping the
//! lowering here means the desktop bridge never has to scrape progress text or
//! invent a second agent lifecycle.

use async_trait::async_trait;
use client::adapter::ClientEventSink;
use client::protocol::events::ClientEvent;
use client::protocol::listings::{SessionAgentMessageRowDto, SessionAgentSummaryDto};
use lingxi_core::types::{ConversationMessage, SessionId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn complete_transcript_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    raw.split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .map(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            line.strip_suffix(b"\r").unwrap_or(line)
        })
}

/// Count valid message records in a transcript prefix.  The raw count is a
/// monotonic watermark even when a visible compact summary is replaced, so a
/// revision-aware renderer cannot mistake a changed snapshot for a duplicate.
#[must_use]
pub fn transcript_revision(raw: &[u8]) -> u64 {
    complete_transcript_lines(raw)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter_map(|value| value.get("message").cloned())
        .filter_map(|message| serde_json::from_value::<ConversationMessage>(message).ok())
        .count() as u64
}

/// Only user-visible conversation records belong in the Agents transcript.
#[must_use]
pub fn conversation_is_visible(message: &ConversationMessage) -> bool {
    !matches!(
        message,
        ConversationMessage::User { is_meta: true, .. }
            | ConversationMessage::User {
                is_compact_summary: true,
                ..
            }
            | ConversationMessage::User {
                is_visible_in_transcript_only: true,
                ..
            }
    )
}

/// Parse one append-only agent transcript, dropping malformed lines and
/// engine-only lifecycle messages.  Truncated final lines are intentionally
/// ignored so a crash during a write does not make the agent unreadable.
#[must_use]
pub fn parse_transcript_messages(raw: &[u8]) -> Vec<ConversationMessage> {
    complete_transcript_lines(raw)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter_map(|value| value.get("message").cloned())
        .filter_map(|message| serde_json::from_value::<ConversationMessage>(message).ok())
        .filter(|message| {
            !matches!(
                message,
                ConversationMessage::System {
                    subtype: Some(subtype),
                    ..
                } if subtype.starts_with("agent_")
            )
        })
        .filter(conversation_is_visible)
        .collect()
}

/// Return the one-based line number of the first malformed COMPLETE record.
/// An unterminated final line is a recoverable interrupted append and is not
/// reported as corruption.
#[must_use]
pub fn first_corrupt_transcript_line(raw: &[u8]) -> Option<usize> {
    complete_transcript_lines(raw)
        .enumerate()
        .find_map(|(index, line)| {
            if line.is_empty() {
                return None;
            }
            let valid = serde_json::from_slice::<serde_json::Value>(line)
                .ok()
                .and_then(|value| value.get("message").cloned())
                .and_then(|message| serde_json::from_value::<ConversationMessage>(message).ok())
                .is_some();
            (!valid).then_some(index + 1)
        })
}

/// Extract an `agent:<uuid>` from a transcript file name.  This rejects
/// arbitrary files and path components before any file content is read.
#[must_use]
pub fn agent_id_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("agent-"))
        .and_then(|id| id.strip_suffix(".jsonl"))
        .and_then(lingxi_core::types::AgentId::parse_prefixed)
        .map(|id| id.to_string())
}

/// Recursively find ordinary, non-symlink agent transcripts.  Workflow runs
/// are nested below `subagents/workflows/<run-id>` and therefore need the
/// recursive walk; symlinks are ignored rather than followed.
pub async fn collect_transcript_paths(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    match tokio::fs::symlink_metadata(root).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session agent transcript root must not be a symlink",
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session agent transcript root is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let mut dirs = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(dir) = dirs.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = tokio::fs::symlink_metadata(&path).await?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                dirs.push(path);
            } else if file_type.is_file() && agent_id_from_path(&path).is_some() {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Securely read one transcript relative to its engine-owned root. The rooted
/// filesystem walks every component without following symlinks, closing the
/// check/open race that a later plain `tokio::fs::read(path)` would re-open.
pub async fn read_transcript(root: &Path, path: &Path) -> std::io::Result<Vec<u8>> {
    let relative = path.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session agent transcript escaped its root",
        )
    })?;
    let root = root.to_path_buf();
    let relative = relative.to_path_buf();
    tokio::task::spawn_blocking(move || {
        lingxi_core::host::rooted_fs::read_to_string(&root, &relative)
            .map(String::into_bytes)
            .map_err(|error| std::io::Error::other(error.to_string()))
    })
    .await
    .map_err(|error| std::io::Error::other(error.to_string()))?
}

/// Locate one validated transcript below the engine-owned root.
pub async fn find_transcript_path(root: &Path, agent_id: &str) -> std::io::Result<Option<PathBuf>> {
    Ok(collect_transcript_paths(root)
        .await?
        .into_iter()
        .find(|path| agent_id_from_path(path).as_deref() == Some(agent_id)))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn activity(message: &ConversationMessage) -> Option<String> {
    let blocks = match message {
        ConversationMessage::Assistant { content, .. }
        | ConversationMessage::User { content, .. } => Some(content.as_slice()),
        ConversationMessage::System { content, .. } => {
            return (!content.is_empty()).then(|| content.chars().take(160).collect());
        }
    }?;
    blocks.iter().find_map(|block| match block {
        lingxi_core::types::ContentBlock::Text { text, .. } if !text.is_empty() => {
            Some(text.chars().take(160).collect())
        }
        lingxi_core::types::ContentBlock::ToolUse { name, .. } => Some(name.clone()),
        lingxi_core::types::ContentBlock::ToolResult { content, .. } if !content.is_empty() => {
            Some(content.chars().take(160).collect())
        }
        _ => None,
    })
}

#[derive(Clone)]
struct BoundAgent {
    session_id: String,
    name: String,
    agent_type: String,
    model: String,
    model_profile: Option<String>,
    persistent: bool,
}

/// Bridges provider-neutral subagent usage into the desktop session's durable
/// cost ledger. The agent crate deliberately exposes this as a host seam so it
/// does not depend on the cost implementation.
pub struct DesktopSubagentUsageRecorder {
    tracker: Arc<cost::CostTracker>,
}

impl DesktopSubagentUsageRecorder {
    #[must_use]
    pub fn new(tracker: Arc<cost::CostTracker>) -> Self {
        Self { tracker }
    }
}

#[async_trait]
impl lingxi_core::host::SubagentUsageRecorder for DesktopSubagentUsageRecorder {
    async fn record_subagent_usage(
        &self,
        session_id: Option<SessionId>,
        model: &str,
        model_profile: Option<&str>,
        usage: lingxi_core::host::SubagentUsage,
        duration: Duration,
        _usage_complete: bool,
    ) {
        if usage.is_zero() {
            return;
        }
        let tracker = session_id.map_or_else(
            || self.tracker.clone(),
            |session_id| self.tracker.scoped(session_id),
        );
        let cache_read = usage.cache_read_input_tokens;
        let cache_write = usage.cache_creation_input_tokens;
        let cost_usage = cost::Usage {
            tokens: cost::TokenUsage {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_write,
                cache_read,
                reasoning_output: usage.reasoning_output_tokens,
                ..Default::default()
            },
            ..Default::default()
        };
        let _ = tracker
            .record_api_response_v2(
                orchestrator::cost_wiring::model_ref_from_string(model, model_profile),
                cost_usage,
                duration,
                0,
                cache_read,
                cache_write,
                false,
                None,
            )
            .await;
    }
}

/// Desktop observer installed by the bridge composition root.  The observer
/// receives the actual typed messages emitted by `PoolSubagentSpawner`; no
/// demo rows or synthetic transcript text enter the client event stream.
pub struct DesktopSessionAgentObserver {
    event_sink: Arc<dyn ClientEventSink>,
    session_id: std::sync::RwLock<String>,
    bound_agents: tokio::sync::Mutex<HashMap<String, BoundAgent>>,
    tool_indexes: tokio::sync::Mutex<HashMap<String, client::adapter::turn::ToolUseIndex>>,
    message_indexes: tokio::sync::Mutex<HashMap<String, u64>>,
    // Process-owned facts, never restored from JSONL. Retain terminal summaries
    // so a concurrent disk read cannot resurrect an older running record.
    observed_agents: std::sync::Mutex<HashMap<String, ObservedAgent>>,
}

/// One agent as THIS process has observed it.
///
/// `terminal` is tracked separately from `agent.status` on purpose. The status
/// is a WIRE label the clients render; whether an agent can still be revived is
/// an engine fact, and the two stopped agreeing once a parked persistent agent
/// started reporting claude-code's `completed` (it renders as `done`) instead of
/// the port's invented `idle`. Reading liveness off the label would have made
/// `Allocated` treat a parked-but-resumable agent as dead and silently refuse to
/// re-register it — a resumed agent that emits nothing, with every test still
/// green because none of them resume one.
#[derive(Clone)]
struct ObservedAgent {
    session_id: String,
    agent: SessionAgentSummaryDto,
    /// The agent reached a real end (`completed` one-shot / `failed` / `killed`)
    /// and must never be re-registered. A parked persistent agent is NOT this.
    terminal: bool,
}

impl DesktopSessionAgentObserver {
    #[must_use]
    pub fn new(event_sink: Arc<dyn ClientEventSink>, session_id: impl Into<String>) -> Self {
        Self {
            event_sink,
            session_id: std::sync::RwLock::new(session_id.into()),
            bound_agents: tokio::sync::Mutex::new(HashMap::new()),
            tool_indexes: tokio::sync::Mutex::new(HashMap::new()),
            message_indexes: tokio::sync::Mutex::new(HashMap::new()),
            observed_agents: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Move the default owner for future allocations after an in-process
    /// session switch. Existing observations keep their original owner.
    pub fn set_session_id(&self, session_id: impl Into<String>) {
        *self
            .session_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session_id.into();
    }

    /// Current process observations for one session. A fresh engine starts
    /// with an empty set even when historical transcripts say "running".
    #[must_use]
    pub fn snapshot(&self, session_id: &str) -> HashMap<String, SessionAgentSummaryDto> {
        self.observed_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, observed)| observed.session_id == session_id)
            .map(|(id, observed)| (id.clone(), observed.agent.clone()))
            .collect()
    }

    fn remember(&self, session_id: &str, agent: &SessionAgentSummaryDto, terminal: bool) {
        self.observed_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                agent.agent_id.clone(),
                ObservedAgent {
                    session_id: session_id.to_string(),
                    agent: agent.clone(),
                    terminal,
                },
            );
    }

    async fn emit_activity(&self, agent_id: lingxi_core::types::AgentId, activity: String) {
        // A workflow retry can arrive through a different observer worker.
        // Check and update under one lock so delayed telemetry cannot revive
        // an idle or terminal child between separate liveness checks.
        let event = {
            let mut agents = self
                .observed_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(observed) = agents.get_mut(&agent_id.to_string()) else {
                return;
            };
            if observed.agent.status != "running" {
                return;
            }
            observed.agent.latest_activity = Some(activity);
            observed.agent.updated_at_ms = Some(unix_time_ms());
            ClientEvent::SessionAgentUpdated {
                session_id: observed.session_id.clone(),
                agent: observed.agent.clone(),
            }
        };
        self.event_sink.emit(event).await;
    }

    async fn emit_observed(&self, event: ClientEvent, terminal: bool) {
        if let ClientEvent::SessionAgentUpdated { session_id, agent } = &event {
            self.remember(session_id, agent, terminal);
        }
        self.event_sink.emit(event).await;
    }

    fn allocated_session_id(
        &self,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> String {
        // The spawner resolves this together with the child's hook and actual
        // transcript directory before allocation crosses an async boundary.
        if let Some(session_id) = origin_session_id {
            return session_id.as_uuid().to_string();
        }
        // Workflow spawns write under the actual parent session's
        // `subagents/workflows/<run>` directory. Prefer that id so nested
        // workers remain fenced to the transcript the bridge opened.
        if let Some(session_id) = agent::workflow_transcript_subdir_override()
            .and_then(|path| path.ancestors().nth(3).map(Path::to_path_buf))
            .and_then(|path| path.file_name().map(|name| name.to_owned()))
            .and_then(|name| name.to_str().map(str::to_owned))
        {
            return session_id;
        }
        self.session_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl lingxi_core::host::subagent_spawn::SubagentSpawnObserver for DesktopSessionAgentObserver {
    fn on_allocated(&self, event: &lingxi_core::host::subagent_spawn::SubagentObservation) {
        if let lingxi_core::host::subagent_spawn::SubagentObservation::Allocated {
            agent_id,
            agent_type,
            name,
            model,
            model_profile,
            origin_session_id,
            ..
        } = event
        {
            self.remember(
                &self.allocated_session_id(*origin_session_id),
                &SessionAgentSummaryDto {
                    agent_id: agent_id.to_string(),
                    name: name.clone().unwrap_or_else(|| agent_type.clone()),
                    agent_type: agent_type.clone(),
                    model: Some(model.clone()),
                    model_profile: model_profile.clone(),
                    status: "running".to_string(),
                    latest_activity: None,
                    updated_at_ms: Some(unix_time_ms()),
                },
                false,
            );
        }
    }

    async fn on_event(&self, event: lingxi_core::host::subagent_spawn::SubagentObservation) {
        use lingxi_core::host::subagent_spawn::SubagentObservation;
        match event {
            SubagentObservation::Allocated {
                agent_id,
                agent_type,
                name,
                model,
                model_profile,
                persistent,
                initial_message_index,
                origin_session_id,
            } => {
                let receipt = self
                    .observed_agents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&agent_id.to_string())
                    .cloned();
                // A REAL end, not merely a `completed` label: a parked
                // persistent agent now reports `completed` too (claude-code's
                // vocabulary — it renders as `done`), and it is precisely the
                // one that must still be allowed to re-register when it is
                // resumed.
                if receipt.as_ref().is_some_and(|observed| observed.terminal) {
                    return;
                }
                let session_id = receipt.map_or_else(
                    || self.allocated_session_id(origin_session_id),
                    |observed| observed.session_id,
                );
                let name = name.unwrap_or_else(|| agent_type.clone());
                self.bound_agents.lock().await.insert(
                    agent_id.to_string(),
                    BoundAgent {
                        session_id: session_id.clone(),
                        name: name.clone(),
                        agent_type: agent_type.clone(),
                        model: model.clone(),
                        model_profile: model_profile.clone(),
                        persistent,
                    },
                );
                self.message_indexes
                    .lock()
                    .await
                    .insert(agent_id.to_string(), initial_message_index);
                self.emit_observed(
                    ClientEvent::SessionAgentUpdated {
                        session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: agent_id.to_string(),
                            name,
                            agent_type,
                            model: Some(model),
                            model_profile,
                            status: "running".to_string(),
                            latest_activity: None,
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    },
                    false,
                )
                .await;
            }
            SubagentObservation::MessageRow {
                agent_id,
                message,
                message_index,
            } => {
                let key = agent_id.to_string();
                let mut indexes = self.message_indexes.lock().await;
                let next = indexes.entry(key).or_default();
                *next = (*next).max(message_index);
                drop(indexes);
                Box::pin(self.on_event(SubagentObservation::Message { agent_id, message })).await;
            }
            SubagentObservation::Message { agent_id, message } => {
                if matches!(&message, ConversationMessage::System { subtype: Some(subtype), .. } if subtype == "agent_idle")
                {
                    // Foreground agents can park after backgrounding their work.
                    // Their event pump stays alive for a later task notification;
                    // settle this turn without dropping its observer binding.
                    self.emit_update(agent_id, "completed", None, false).await;
                    return;
                }
                if !conversation_is_visible(&message) {
                    // A parked worker can wake on a task-notification input.
                    // Publish the lifecycle edge, never its hidden contents —
                    // but carry the remembered activity forward, because
                    // `remember` replaces the record wholesale and `None` here
                    // would erase whatever `Progress`/`Retry` just published.
                    let latest_activity = self
                        .observed_agents
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&agent_id.to_string())
                        .and_then(|observed| observed.agent.latest_activity.clone());
                    self.emit_update(agent_id, "running", latest_activity, false)
                        .await;
                    return;
                }
                let key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&key).cloned() else {
                    // An observer event from a stale/foreign runtime must not
                    // leak into this session's feed.
                    return;
                };
                let dto = {
                    let mut indexes = self.tool_indexes.lock().await;
                    let index = indexes.entry(key.clone()).or_default();
                    client::adapter::lowering::lower_conversation_message_with(&message, index)
                };
                let message_index = {
                    let mut indexes = self.message_indexes.lock().await;
                    let next = indexes.entry(key.clone()).or_default();
                    let current = *next;
                    *next = next.saturating_add(1);
                    current
                };
                self.event_sink
                    .emit(ClientEvent::SessionAgentMessage {
                        session_id: bound.session_id.clone(),
                        agent_id: key.clone(),
                        row: SessionAgentMessageRowDto {
                            message_index,
                            message_uuid: message.id().as_uuid().to_string(),
                            message: dto,
                            api_error_json: None,
                        },
                    })
                    .await;
                self.emit_observed(
                    ClientEvent::SessionAgentUpdated {
                        session_id: bound.session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: key,
                            name: bound.name,
                            agent_type: bound.agent_type,
                            model: Some(bound.model),
                            model_profile: bound.model_profile,
                            status: "running".to_string(),
                            latest_activity: activity(&message),
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    },
                    false,
                )
                .await;
            }
            SubagentObservation::ServerFallbackTombstone {
                agent_id,
                message,
                display_only,
            } => {
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                self.event_sink
                    .emit(ClientEvent::SessionAgentTombstone {
                        session_id: bound.session_id,
                        agent_id: agent_key,
                        message_uuid: message.uuid.as_uuid().to_string(),
                        display_only,
                    })
                    .await;
            }
            SubagentObservation::ServerFallbackApiErrorRow {
                agent_id,
                row,
                message_index,
            } => {
                // Keep the full API-error envelope in the typed observer event
                // until this host boundary; the session-agent message DTO is
                // only the visible conversation projection.
                let message = row.query_message();
                let key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&key).cloned() else {
                    return;
                };
                let dto = {
                    let mut indexes = self.tool_indexes.lock().await;
                    let index = indexes.entry(key.clone()).or_default();
                    client::adapter::lowering::lower_conversation_message_with(&message, index)
                };
                let message_index = {
                    let mut indexes = self.message_indexes.lock().await;
                    let next = indexes.entry(key.clone()).or_default();
                    *next = (*next).max(message_index.saturating_add(1));
                    message_index
                };
                self.event_sink
                    .emit(ClientEvent::SessionAgentMessage {
                        session_id: bound.session_id,
                        agent_id: key,
                        row: SessionAgentMessageRowDto {
                            message_index,
                            message_uuid: row.uuid.as_uuid().to_string(),
                            message: dto,
                            api_error_json: Some(
                                serde_json::to_string(&row)
                                    .expect("fallback API-error row serializes"),
                            ),
                        },
                    })
                    .await;
            }
            SubagentObservation::Completed { agent_id, .. } => {
                let persistent = self
                    .bound_agents
                    .lock()
                    .await
                    .get(&agent_id.to_string())
                    .is_some_and(|bound| bound.persistent);
                // claude-code has no `idle` status for a finished background
                // agent: its task row is `completed` and renders as `(done)`
                // (`quietlyParked` picks `(parked)` instead, and an unsurfaced
                // completion adds `, unread`). `idle` there is the name of the
                // FOOTER GROUP such rows collapse into, and of a teammate's
                // state — the port borrowed the group's word for a row's
                // status, which is why one finished agent showed as `idle` on
                // the Subagents row and `completed` on its own task row.
                //
                // A persistent agent still differs from a one-shot one: it
                // keeps its binding so a later message can resume it. That
                // difference now lives in `clear_state` alone, where it is a
                // fact about the engine rather than a word the user reads.
                self.emit_update(agent_id, "completed", None, !persistent)
                    .await;
            }
            SubagentObservation::Failed { agent_id, error } => {
                self.emit_terminal_with_activity(agent_id, "failed", Some(error))
                    .await;
            }
            SubagentObservation::Killed { agent_id } => {
                self.emit_terminal(agent_id, "killed").await
            }
            SubagentObservation::Progress {
                agent_id,
                tool_use_count,
                token_count,
            } => {
                self.emit_activity(
                    agent_id,
                    format!("{tool_use_count} tool uses · {token_count} tokens"),
                )
                .await;
            }
            SubagentObservation::Retry {
                agent_id,
                attempt,
                reason,
            } => {
                self.emit_activity(
                    agent_id,
                    format!("Retrying (attempt {attempt}): {reason}")
                        .chars()
                        .take(160)
                        .collect(),
                )
                .await;
            }
        }
    }
}

impl DesktopSessionAgentObserver {
    async fn emit_terminal(&self, agent_id: lingxi_core::types::AgentId, status: &str) {
        self.emit_terminal_with_activity(agent_id, status, None)
            .await;
    }

    async fn emit_terminal_with_activity(
        &self,
        agent_id: lingxi_core::types::AgentId,
        status: &str,
        latest_activity: Option<String>,
    ) {
        self.emit_update(agent_id, status, latest_activity, true)
            .await;
    }

    async fn emit_update(
        &self,
        agent_id: lingxi_core::types::AgentId,
        status: &str,
        latest_activity: Option<String>,
        clear_state: bool,
    ) {
        let key = agent_id.to_string();
        let Some(bound) = self.bound_agents.lock().await.get(&key).cloned() else {
            // before_start may fail after the synchronous allocation receipt
            // but before asynchronous Allocated delivery creates the binding.
            if clear_state {
                let receipt = self
                    .observed_agents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&key)
                    .cloned();
                if let Some(observed) = receipt {
                    let mut agent = observed.agent;
                    agent.status = status.to_string();
                    agent.latest_activity = latest_activity;
                    agent.updated_at_ms = Some(unix_time_ms());
                    self.emit_observed(
                        ClientEvent::SessionAgentUpdated {
                            session_id: observed.session_id,
                            agent,
                        },
                        clear_state,
                    )
                    .await;
                }
            }
            return;
        };
        self.emit_observed(
            ClientEvent::SessionAgentUpdated {
                session_id: bound.session_id,
                agent: SessionAgentSummaryDto {
                    agent_id: key.clone(),
                    name: bound.name,
                    agent_type: bound.agent_type,
                    model: Some(bound.model),
                    model_profile: bound.model_profile,
                    status: status.to_string(),
                    latest_activity,
                    updated_at_ms: Some(unix_time_ms()),
                },
            },
            clear_state,
        )
        .await;
        if clear_state {
            // Terminal rows remain in the client roster, but one-shot/failed/
            // killed children must not retain per-agent indexes forever.
            self.bound_agents.lock().await.remove(&key);
            self.tool_indexes.lock().await.remove(&key);
            self.message_indexes.lock().await.remove(&key);
        }
    }
}

/// Lower a complete transcript to the event's message DTO shape.
#[must_use]
pub fn lower_transcript(raw: &[u8]) -> Vec<client::protocol::message::MessageDto> {
    client::adapter::lowering::lower_transcript(&parse_transcript_messages(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::subagent_spawn::{
        SubagentObservation, SubagentSpawnObserver, SubagentUsage, SubagentUsageRecorder,
    };

    async fn allocate(
        observer: &DesktopSessionAgentObserver,
        agent_id: lingxi_core::types::AgentId,
        persistent: bool,
        initial_message_index: u64,
    ) {
        observer
            .on_event(SubagentObservation::Allocated {
                agent_id,
                agent_type: "researcher".to_string(),
                name: Some("Research".to_string()),
                model: "test-model".to_string(),
                model_profile: Some("test-profile".to_string()),
                persistent,
                initial_message_index,
                origin_session_id: None,
            })
            .await;
    }

    fn completed(agent_id: lingxi_core::types::AgentId) -> SubagentObservation {
        SubagentObservation::Completed {
            agent_id,
            content: serde_json::json!("done"),
            usage: SubagentUsage::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            assistant_message_count: 0,
            last_request_id: None,
        }
    }

    #[tokio::test]
    async fn live_snapshot_is_process_scoped_and_allocation_precedes_async_delivery() {
        let observer =
            DesktopSessionAgentObserver::new(client::adapter::MockSink::arc(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        let allocation = SubagentObservation::Allocated {
            agent_id,
            agent_type: "reviewer".into(),
            name: Some("code-review".into()),
            model: "test-model".into(),
            model_profile: None,
            persistent: false,
            initial_message_index: 0,
            origin_session_id: None,
        };
        observer.on_allocated(&allocation);
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "running"
        );
        assert!(observer.snapshot("session-b").is_empty());
        // Reconnect uses this same instance; restart owns a fresh empty instance.
        assert_eq!(observer.snapshot("session-a").len(), 1);
        let restarted =
            DesktopSessionAgentObserver::new(client::adapter::MockSink::arc(), "session-a");
        assert!(restarted.snapshot("session-a").is_empty());
        observer
            .on_event(SubagentObservation::Killed { agent_id })
            .await;
        observer.on_event(allocation).await; // Late allocation cannot revive a cancelled startup.
        observer
            .on_event(SubagentObservation::Killed { agent_id })
            .await;
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "killed"
        );
    }

    #[tokio::test]
    async fn session_switch_keeps_existing_receipts_and_moves_future_allocations() {
        let observer =
            DesktopSessionAgentObserver::new(client::adapter::MockSink::arc(), "session-a");
        let old_id = lingxi_core::types::AgentId::new();
        let event = SubagentObservation::Allocated {
            agent_id: old_id,
            agent_type: "reviewer".into(),
            name: None,
            model: "test-model".into(),
            model_profile: None,
            persistent: false,
            initial_message_index: 0,
            origin_session_id: None,
        };
        observer.on_allocated(&event);
        observer.set_session_id("session-b");
        observer.on_event(event).await;
        let new_id = lingxi_core::types::AgentId::new();
        allocate(&observer, new_id, false, 0).await;
        assert!(
            observer
                .snapshot("session-a")
                .contains_key(&old_id.to_string())
        );
        assert!(
            !observer
                .snapshot("session-b")
                .contains_key(&old_id.to_string())
        );
        assert!(
            observer
                .snapshot("session-b")
                .contains_key(&new_id.to_string())
        );
    }

    #[tokio::test]
    async fn explicit_spawn_owner_survives_session_switch_and_delayed_delivery() {
        let sink = client::adapter::MockSink::arc();
        let session_a = lingxi_core::types::SessionId::new();
        let session_b = lingxi_core::types::SessionId::new();
        let owner_a = session_a.as_uuid().to_string();
        let owner_b = session_b.as_uuid().to_string();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), &owner_a);
        observer.set_session_id(&owner_b);

        // The old background parent creates a child after the switch. Its
        // explicit origin wins over the observer's current-session fallback.
        for (owner, synchronous_receipt) in [(session_a, true), (session_b, false)] {
            let agent_id = lingxi_core::types::AgentId::new();
            let allocation = SubagentObservation::Allocated {
                agent_id,
                agent_type: "reviewer".into(),
                name: None,
                model: "test-model".into(),
                model_profile: None,
                persistent: false,
                initial_message_index: 0,
                origin_session_id: Some(owner),
            };
            if synchronous_receipt {
                observer.on_allocated(&allocation);
            }
            // A subsequent switch before async delivery cannot change ownership,
            // including when the synchronous receipt was not available.
            observer.set_session_id("session-c");
            observer.on_event(allocation).await;
            observer.on_event(completed(agent_id)).await;
            let expected = owner.as_uuid().to_string();
            assert_eq!(
                observer.snapshot(&expected)[&agent_id.to_string()].status,
                "completed"
            );
            assert!(observer.snapshot("session-c").is_empty());
            assert!(sink.events().await.iter().any(|event| matches!(
                event,
                ClientEvent::SessionAgentUpdated { session_id, agent }
                    if session_id == &expected && agent.agent_id == agent_id.to_string()
                        && agent.status == "completed"
            )));
        }
        assert_eq!(observer.snapshot(&owner_a).len(), 1);
        assert_eq!(observer.snapshot(&owner_b).len(), 1);
    }

    #[tokio::test]
    async fn progress_and_retry_are_live_activity_not_a_resurrection_signal() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, true, 0).await;
        observer
            .on_event(SubagentObservation::Progress {
                agent_id,
                tool_use_count: 3,
                token_count: 42,
            })
            .await;
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()]
                .latest_activity
                .as_deref(),
            Some("3 tool uses · 42 tokens")
        );
        observer
            .on_event(SubagentObservation::Retry {
                agent_id,
                attempt: 2,
                reason: "waiting for response".into(),
            })
            .await;
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()]
                .latest_activity
                .as_deref(),
            Some("Retrying (attempt 2): waiting for response")
        );
        observer.on_event(completed(agent_id)).await;
        let events = sink.events().await.len();
        observer
            .on_event(SubagentObservation::Progress {
                agent_id,
                tool_use_count: 4,
                token_count: 43,
            })
            .await;
        observer
            .on_event(SubagentObservation::Retry {
                agent_id,
                attempt: 3,
                reason: "late".into(),
            })
            .await;
        assert_eq!(sink.events().await.len(), events);
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "completed"
        );
    }

    #[tokio::test]
    async fn foreground_park_updates_liveness_without_losing_wake_binding() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, false, 0).await;
        for _ in 0..2 {
            let before = sink.events().await.len();
            observer
                .on_event(SubagentObservation::Message {
                    agent_id,
                    message: ConversationMessage::System { api_system: None,
                        id: lingxi_core::types::MessageId::new(),
                        content: "idle".into(),
                        subtype: Some("agent_idle".into()),
                        compact_metadata: None,
                        model_fallback: None,
                        refusal_fallback: None,
                    },
                })
                .await;
            assert_eq!(
                observer.snapshot("session-a")[&agent_id.to_string()].status,
                "completed"
            );
            let events = sink.events().await;
            assert!(
                matches!(&events[before..], [ClientEvent::SessionAgentUpdated { agent, .. }] if agent.status == "completed")
            );
            observer
                .on_event(SubagentObservation::Message {
                    agent_id,
                    message: ConversationMessage::user_meta(
                        lingxi_core::types::MessageId::new(),
                        "task finished".into(),
                    ),
                })
                .await;
            assert_eq!(
                observer.snapshot("session-a")[&agent_id.to_string()].status,
                "running"
            );
        }
        observer
            .on_event(SubagentObservation::Killed { agent_id })
            .await;
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "killed"
        );
    }

    #[tokio::test]
    async fn hidden_notification_wakes_idle_without_exposing_internal_input() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, true, 0).await;
        observer.on_event(completed(agent_id)).await;
        let before = sink.events().await.len();
        observer
            .on_event(SubagentObservation::Message {
                agent_id,
                message: ConversationMessage::user_meta(
                    lingxi_core::types::MessageId::new(),
                    "private task notification".into(),
                ),
            })
            .await;
        let events = sink.events().await;
        assert!(
            matches!(&events[before..], [ClientEvent::SessionAgentUpdated { agent, .. }] if agent.status == "running" && agent.latest_activity.is_none())
        );
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "running"
        );
    }

    #[test]
    fn agent_id_parser_rejects_non_agent_files() {
        assert!(agent_id_from_path(Path::new("agent-nope.jsonl")).is_none());
        assert!(
            agent_id_from_path(Path::new(
                "agent-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl"
            ))
            .is_some()
        );
        assert!(
            agent_id_from_path(Path::new(
                "agent-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.task.json"
            ))
            .is_none()
        );
    }

    #[test]
    fn revision_counts_hidden_records_while_lowering_only_visible() {
        let visible =
            ConversationMessage::user(lingxi_core::types::MessageId::new(), "hello".to_string());
        let hidden = ConversationMessage::User { api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: "summary".into(),
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: true,
            is_visible_in_transcript_only: false,
        };
        let raw = format!(
            "{}\n{}\n",
            serde_json::json!({"message": visible}),
            serde_json::json!({"message": hidden})
        );
        assert_eq!(transcript_revision(raw.as_bytes()), 2);
        assert_eq!(parse_transcript_messages(raw.as_bytes()).len(), 1);
    }

    #[test]
    fn transcript_validation_ignores_only_an_unterminated_tail() {
        let message =
            ConversationMessage::user(lingxi_core::types::MessageId::new(), "hello".to_string());
        let valid = serde_json::json!({"message": message}).to_string();
        let raw = format!("{valid}\nnot-json\n");
        assert_eq!(first_corrupt_transcript_line(raw.as_bytes()), Some(2));
        assert_eq!(
            first_corrupt_transcript_line(format!("{valid}\nnot-json").as_bytes()),
            None,
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transcript_walk_and_read_reject_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root_parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = root_parent.path().join("subagents");
        std::fs::create_dir(&root).unwrap();
        let agent_id = lingxi_core::types::AgentId::new();
        let outside_file = outside.path().join(format!("agent-{agent_id}.jsonl"));
        std::fs::write(&outside_file, "secret").unwrap();
        symlink(&outside_file, root.join(format!("agent-{agent_id}.jsonl"))).unwrap();

        assert!(collect_transcript_paths(&root).await.unwrap().is_empty());
        assert!(
            read_transcript(&root, &root.join(format!("agent-{agent_id}.jsonl")),)
                .await
                .is_err()
        );

        let linked_root = root_parent.path().join("linked-subagents");
        symlink(outside.path(), &linked_root).unwrap();
        assert!(collect_transcript_paths(&linked_root).await.is_err());
    }

    #[tokio::test]
    async fn allocation_origin_routes_updates_independently_of_boot_session() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "boot-session");
        for owner in [
            lingxi_core::types::SessionId::new(),
            lingxi_core::types::SessionId::new(),
        ] {
            let agent_id = lingxi_core::types::AgentId::new();
            observer
                .on_event(SubagentObservation::Allocated {
                    agent_id,
                    agent_type: "reviewer".into(),
                    name: None,
                    model: "test-model".into(),
                    model_profile: None,
                    persistent: false,
                    initial_message_index: 0,
                    origin_session_id: Some(owner),
                })
                .await;
            observer.on_event(completed(agent_id)).await;
            let expected = owner.as_uuid().to_string();
            let events = sink.events().await;
            let updates: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    ClientEvent::SessionAgentUpdated { session_id, agent }
                        if agent.agent_id == agent_id.to_string() =>
                    {
                        Some((session_id, &agent.status))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(updates.len(), 2);
            assert!(
                updates
                    .iter()
                    .all(|(session_id, _)| *session_id == &expected)
            );
            assert_eq!(updates[1].1, "completed");
        }
    }

    #[tokio::test]
    async fn one_shot_completion_emits_completed_and_releases_all_observer_state() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, false, 0).await;

        observer.on_event(completed(agent_id)).await;

        assert!(sink.events().await.iter().any(|event| matches!(
            event,
            ClientEvent::SessionAgentUpdated { agent, .. }
                if agent.agent_id == agent_id.to_string() && agent.status == "completed"
        )));
        assert!(observer.bound_agents.lock().await.is_empty());
        assert!(observer.tool_indexes.lock().await.is_empty());
        assert!(observer.message_indexes.lock().await.is_empty());
    }

    /// A PARKED persistent agent must still be able to come back.
    ///
    /// The allocation gate used to read liveness off the status STRING
    /// (`"killed" | "failed" | "cancelled" | "completed"`). The moment a parked
    /// persistent agent started reporting claude-code's `completed`, that gate
    /// would have swallowed its resume: `Allocated` returns early, the agent
    /// never re-registers, and it emits nothing for the rest of the session —
    /// with every other test still green, because none of them resume one.
    /// Hence the separate `terminal` flag, and hence this test.
    #[tokio::test]
    async fn a_parked_persistent_agent_can_be_reallocated() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, true, 0).await;
        observer.on_event(completed(agent_id)).await;
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "completed",
            "precondition: parking reports claude-code's completed, the exact \
             value the old string gate treated as dead",
        );

        allocate(&observer, agent_id, true, 0).await;

        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "running",
            "a resumed persistent agent must re-register",
        );
        assert!(
            observer
                .bound_agents
                .lock()
                .await
                .contains_key(&agent_id.to_string())
        );
    }

    /// The other half of the same gate: a one-shot agent that really ended must
    /// NOT be revived by a late or duplicate `Allocated`. Both sides have to be
    /// pinned — a flag that is always false would pass the test above on its own.
    #[tokio::test]
    async fn a_terminal_agent_is_never_reallocated() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, false, 0).await;
        observer.on_event(completed(agent_id)).await;
        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "completed"
        );

        allocate(&observer, agent_id, false, 0).await;

        assert_eq!(
            observer.snapshot("session-a")[&agent_id.to_string()].status,
            "completed",
            "a finished one-shot agent stays finished",
        );
        assert!(observer.bound_agents.lock().await.is_empty());
    }

    #[tokio::test]
    async fn persistent_completion_is_completed_and_keeps_restored_index_for_resume() {
        let sink = client::adapter::MockSink::arc();
        let observer = DesktopSessionAgentObserver::new(sink.clone(), "session-a");
        let agent_id = lingxi_core::types::AgentId::new();
        allocate(&observer, agent_id, true, 4).await;

        observer.on_event(completed(agent_id)).await;
        assert!(sink.events().await.iter().any(|event| matches!(
            event,
            ClientEvent::SessionAgentUpdated { agent, .. }
                // claude-code's word for a finished background agent, parked
                // or not — it renders as `(done)`. `idle` named the footer
                // GROUP, never the row.
                if agent.agent_id == agent_id.to_string() && agent.status == "completed"
        )));
        assert!(
            observer
                .bound_agents
                .lock()
                .await
                .contains_key(&agent_id.to_string())
        );
        assert_eq!(
            observer
                .message_indexes
                .lock()
                .await
                .get(&agent_id.to_string()),
            Some(&4)
        );

        observer
            .on_event(SubagentObservation::Message {
                agent_id,
                message: ConversationMessage::Assistant { per_turn_effort: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![lingxi_core::types::ContentBlock::Text {
                        text: "resumed output".to_string(),
                        citations: None,
                    }],
                    stop_reason: None,
                },
            })
            .await;
        assert!(sink.events().await.iter().any(|event| matches!(
            event,
            ClientEvent::SessionAgentMessage { agent_id: emitted, row, .. }
                if emitted == &agent_id.to_string() && row.message_index == 4
        )));
        assert!(sink.events().await.iter().any(|event| matches!(
            event,
            ClientEvent::SessionAgentUpdated { agent, .. }
                if agent.agent_id == agent_id.to_string() && agent.status == "running"
        )));

        observer
            .on_event(SubagentObservation::Killed { agent_id })
            .await;
        assert!(observer.bound_agents.lock().await.is_empty());
        assert!(observer.message_indexes.lock().await.is_empty());
    }

    #[tokio::test]
    async fn desktop_usage_recorder_adds_subagent_tokens_to_session_ledger() {
        let session_id = lingxi_core::types::SessionId::new();
        let tracker = Arc::new(cost::CostTracker::new(
            session_id,
            Arc::new(cost::PricingCatalog::builtin_reference()),
            tokio::sync::mpsc::channel(1).0,
        ));
        let recorder = DesktopSubagentUsageRecorder::new(tracker.clone());

        recorder
            .record_subagent_usage(
                Some(session_id),
                "claude-opus-4-8",
                None,
                SubagentUsage {
                    total_tokens: 26,
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 5,
                    cache_read_input_tokens: 3,
                    reasoning_output_tokens: 55,
                },
                Duration::from_millis(12),
                true,
            )
            .await;

        let snapshot = tracker.scoped(session_id).snapshot().await;
        let usage = snapshot
            .per_model_usage
            .values()
            .next()
            .expect("subagent response should create a model usage row")
            .usage
            .tokens;
        assert_eq!(usage.input, 11);
        assert_eq!(usage.output, 7);
        assert_eq!(usage.cache_write, 5);
        assert_eq!(usage.cache_read, 3);
        assert_eq!(usage.reasoning_output, 55);
    }
}
