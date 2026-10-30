//! Durable app-task admission. Native hosts derive Owner from the active run,
//! never from a page message. Prompt/images/credentials are deliberately absent
//! from persisted records. All methods are synchronous; call off the UI thread.
mod chat_store;
pub mod protocol;
mod recovery;
pub use chat_store::{ChatListPage, ChatMessageMetadata, ChatStore};
pub use recovery::ChatRecoveryFrontier;

use super::AppError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard,
    },
};

const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_ATTACHMENT_BASE64_BYTES: usize = ((MAX_ATTACHMENT_BYTES + 2) / 3) * 4;
const MAX_PROTOCOL_EVENT_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_NATIVE_OUTPUT_BYTES: u64 = 9_007_199_254_740_991;
const MAX_PROTOCOL_EVENT_PAGE: usize = 128;
const PROTOCOL_EVENT_STORAGE_FAILED: &str = "protocol_event_storage_failed";
const PROTOCOL_TERMINAL_CONFLICT: &str = "protocol_terminal_conflict";
const PROTOCOL_COMMIT_INCOMPLETE: &str = "protocol_commit_incomplete";
const IOS_BACKGROUND_EXPIRED: &str = "ios_background_expired";

#[derive(Debug)]
enum EventLogReadError {
    Corrupt(&'static str),
    Unavailable(AppError),
}

impl From<EventLogReadError> for AppError {
    fn from(error: EventLogReadError) -> Self {
        match error {
            EventLogReadError::Corrupt(message) => invalid(message),
            EventLogReadError::Unavailable(error) => error,
        }
    }
}

fn protocol_schema_unavailable() -> AppError {
    AppError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        "The pinned AG-UI event schema is unavailable",
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Owner {
    pub app_id: String,
    pub release_digest: String,
    pub invocation_id: String,
}
impl Owner {
    pub fn validate(&self) -> Result<(), AppError> {
        if !valid_app_id(&self.app_id)
            || !valid_hex(&self.release_digest, 64)
            || self.invocation_id.len() > 128
            || self.invocation_id.is_empty()
            || self.invocation_id.chars().any(char::is_control)
        {
            return Err(AppError::Invalid("invalid Agent task owner".into()));
        }
        Ok(())
    }
}

fn valid_app_id(app_id: &str) -> bool {
    app_id.starts_with('a') && valid_hex(app_id.get(1..).unwrap_or_default(), 32)
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ToolGrant {
    Builtin {
        name: String,
    },
    Mcp {
        #[serde(rename = "serverConfigId")]
        server_config_id: String,
        #[serde(rename = "configRevision")]
        config_revision: String,
        #[serde(rename = "rawToolName")]
        raw_tool_name: String,
        #[serde(rename = "definitionDigest")]
        definition_digest: String,
    },
}
/// Exact public IDs; normalized MCP tool names are never authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Model {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub model: Model,
    pub tools: BTreeMap<String, ToolGrant>,
    /// Host-approved, release-declared Skill IDs. Skills do not add tools.
    #[serde(default)]
    pub skills: Vec<String>,
    pub background: bool,
    pub max_turns: u32,
    pub timeout_ms: u64,
}
impl Settings {
    fn validate(&self) -> Result<(), AppError> {
        if self.model.provider_id.trim().is_empty()
            || self.model.model_id.trim().is_empty()
            || self.max_turns == 0
            || self.max_turns > 100
            || self.timeout_ms == 0
            || self.timeout_ms > 3_600_000
            || self.tools.len() > 128
            || self.skills.len() > 128
            || self.skills.iter().any(|skill| !valid_skill_id(skill))
            || self
                .skills
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.skills.len()
        {
            return Err(AppError::Invalid("invalid host Agent settings".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    pub request_id: String,
    /// Public AG-UI thread identity, kept inside the private task ledger so a
    /// duplicate runId cannot be replayed against another thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_input_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_message_sha256: Option<String>,
    /// Host-only snapshot of the committed Chat history used for normalization.
    /// Never accepted from page JSON or included in a persisted request digest.
    #[serde(skip)]
    pub protocol_history_digest: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Ordered content produced by the public AG-UI normalizer. Native model
    /// adapters also accept the bounded private prompt/attachments DTO.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_parts: Vec<InputContentPart>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum InputContentPart {
    Text {
        text: String,
    },
    Attachment {
        name: String,
        #[serde(rename = "mediaType")]
        media_type: String,
        base64: String,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub name: String,
    pub media_type: String,
    pub base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentMetadata {
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

fn deserialize_present_json<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskResult {
    #[serde(default)]
    pub text: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<Value>,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskProgress {
    /// Confirmed completed model turns, when known; not a completion percent.
    pub completed_model_turns: u32,
    /// Configured turn ceiling, not an estimate of total work.
    pub turn_limit: u32,
}

/// Host-private evidence for the exact sealed transcript used by one Chat
/// turn. This is never included in the public Task DTO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatTranscriptCheckpoint {
    pub byte_len: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
impl State {
    pub fn terminal(&self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Task {
    pub run_id: String,
    pub request_id: String,
    pub state: State,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: Model,
    pub limits: Limits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskError>,
}

/// App/release-scoped task page returned to the mobile Agent host. Cursor
/// tokens are the opaque public request IDs of the last included task; hosts
/// translate them back to private store IDs before reading the next page.
#[derive(Clone, Debug)]
pub struct TaskListPage {
    /// Tasks in the current stable-order page.
    pub tasks: Vec<Task>,
    /// Public request ID of the last task when another page is available.
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub max_turns: u32,
    pub timeout_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatState {
    Idle,
    Running,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSummary {
    pub chat_id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub archived: bool,
    pub state: ChatState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Grant {
    owner: Owner,
    settings: Settings,
    epoch: u64,
}

/// Trusted native stop intent; never accepted from the browser protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StopIntent {
    IosBackgroundExpired,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    owner: Owner,
    input_digest: String,
    epoch: u64,
    cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop_intent: Option<StopIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_input_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_transcript_checkpoint: Option<ChatTranscriptCheckpoint>,
    // Trusted-host observation only: never serialized as part of the public Task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_output: Option<NativeOutputObservation>,
    /// Durable intent written before publishing Chat or protocol output. A
    /// surviving intent is never evidence that final publication committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_commit_pending: Option<Task>,
    /// Private candidate retained when recovery detects a terminal conflict.
    /// It is evidence only and is never returned as a task result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_candidate_evidence: Option<Task>,
    task: Task,
}

fn protocol_storage_failed(record: &Record) -> bool {
    record.task.error.as_ref().is_some_and(|error| {
        matches!(
            error.code.as_str(),
            PROTOCOL_EVENT_STORAGE_FAILED | PROTOCOL_TERMINAL_CONFLICT | PROTOCOL_COMMIT_INCOMPLETE
        )
    })
}

fn mark_protocol_failure(record: &mut Record, code: &str) {
    if record.task.result.is_some() {
        record.protocol_candidate_evidence = Some(record.task.clone());
    }
    record.task.state = State::Interrupted;
    record.task.phase = None;
    record.task.result = None;
    record.task.error = Some(TaskError {
        code: code.into(),
        message: error_message(code).into(),
    });
    record.task.sequence = record.task.sequence.saturating_add(1);
    record.task.updated_at = now();
}

fn mark_protocol_storage_failed(record: &mut Record) {
    mark_protocol_failure(record, PROTOCOL_EVENT_STORAGE_FAILED);
}

fn terminal_candidate(
    record: &Record,
    authorized: bool,
    state: State,
    result: Option<TaskResult>,
    usage: Option<Value>,
    error: Option<String>,
) -> Result<Task, AppError> {
    if matches!(state, State::Queued) || (state != State::Succeeded && result.is_some()) {
        return Err(invalid("invalid Agent state transition"));
    }
    let (state, result, error) = if state == State::Succeeded && !authorized {
        (
            State::Cancelled,
            None,
            Some(
                if record.cancel_requested {
                    "cancelled"
                } else {
                    "authorization_revoked"
                }
                .into(),
            ),
        )
    } else {
        (state, result, error)
    };
    let (state, result, error) = if state == State::Cancelled
        && record.stop_intent == Some(StopIntent::IosBackgroundExpired)
    {
        (
            State::Interrupted,
            None,
            Some(IOS_BACKGROUND_EXPIRED.into()),
        )
    } else {
        (state, result, error)
    };
    let (state, result, error) = if record.protocol_thread_id.is_some()
        && state == State::Succeeded
        && result
            .as_ref()
            .and_then(|result| result.output.as_ref())
            .is_some_and(Value::is_null)
    {
        (
            State::Failed,
            None,
            Some("structured_output_not_representable".into()),
        )
    } else {
        (state, result, error)
    };
    let error = if matches!(state, State::Failed | State::Interrupted) {
        error.or_else(|| Some("agent_failed".into()))
    } else {
        error
    };

    let mut task = record.task.clone();
    task.state = state;
    task.phase = None;
    task.sequence = task.sequence.saturating_add(1);
    task.updated_at = now();
    task.result = result;
    task.usage = usage;
    task.error = error.map(|code| TaskError {
        message: error_message(&code).into(),
        code,
    });
    Ok(task)
}

fn pending_commit_task(candidate: &Task) -> Task {
    let mut task = candidate.clone();
    task.state = State::Interrupted;
    task.phase = None;
    task.result = None;
    task.error = Some(TaskError {
        code: PROTOCOL_COMMIT_INCOMPLETE.into(),
        message: error_message(PROTOCOL_COMMIT_INCOMPLETE).into(),
    });
    task
}

fn terminal_matches_task(record: &Record, event: &Value) -> bool {
    let event_type = event["type"].as_str();
    let outcome = event
        .get("outcome")
        .and_then(|value| value["type"].as_str());
    match record.task.state {
        State::Succeeded => {
            event_type == Some("RUN_FINISHED")
                && (event.get("outcome").is_none() || outcome == Some("success"))
                && !event.get("result").is_some_and(Value::is_null)
                && record.task.result.as_ref().is_some_and(|result| {
                    !result.output.as_ref().is_some_and(Value::is_null)
                        && result.output.as_ref() == event.get("result")
                })
        }
        State::Cancelled => event_type == Some("RUN_FINISHED") && outcome == Some("cancelled"),
        State::Failed | State::Interrupted => {
            let event_code = event.get("code").and_then(Value::as_str);
            let task_code = record.task.error.as_ref().map(|error| error.code.as_str());
            event_type == Some("RUN_ERROR")
                && task_code.map_or(event_code == Some("agent_failed"), |code| {
                    event_code == Some(code)
                })
        }
        State::Queued | State::Running => false,
    }
}

fn validate_pending_commit_event(record: &Record, event: &Value) -> Result<(), AppError> {
    let candidate = record
        .protocol_commit_pending
        .as_ref()
        .ok_or_else(|| invalid("Agent task has no pending terminal commit"))?;
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "TEXT_MESSAGE_START" | "TEXT_MESSAGE_CONTENT" | "TEXT_MESSAGE_END" => {
            let result = candidate
                .result
                .as_ref()
                .filter(|_| candidate.state == State::Succeeded)
                .ok_or_else(|| invalid("non-success task cannot publish assistant text"))?;
            if event.get("messageId").and_then(Value::as_str) != result.message_id.as_deref() {
                return Err(invalid(
                    "AG-UI assistant message does not match pending task",
                ));
            }
            if event_type == "TEXT_MESSAGE_START"
                && event.get("role").and_then(Value::as_str) != Some("assistant")
            {
                return Err(invalid("AG-UI assistant role does not match pending task"));
            }
            if event_type == "TEXT_MESSAGE_CONTENT"
                && event.get("delta").and_then(Value::as_str).is_none()
            {
                return Err(invalid("AG-UI assistant content is invalid"));
            }
            Ok(())
        }
        "RUN_FINISHED" | "RUN_ERROR" => {
            let mut candidate_record = record.clone();
            candidate_record.task = candidate.clone();
            if terminal_matches_task(&candidate_record, event) {
                Ok(())
            } else {
                Err(invalid("AG-UI terminal event does not match pending task"))
            }
        }
        _ => Err(invalid("unsupported AG-UI event during terminal commit")),
    }
}

fn complete_protocol_commit_matches(record: &Record, events: &[SequencedEvent]) -> bool {
    let Some(candidate) = record.protocol_commit_pending.as_ref() else {
        return false;
    };
    if !events.last().is_some_and(|entry| {
        is_terminal_event(&entry.event) && {
            let mut candidate_record = record.clone();
            candidate_record.task = candidate.clone();
            terminal_matches_task(&candidate_record, &entry.event)
        }
    }) {
        return false;
    }

    let text_events = events
        .iter()
        .filter(|entry| {
            matches!(
                entry.event.get("type").and_then(Value::as_str),
                Some("TEXT_MESSAGE_START" | "TEXT_MESSAGE_CONTENT" | "TEXT_MESSAGE_END")
            )
        })
        .map(|entry| &entry.event)
        .collect::<Vec<_>>();
    if candidate.state != State::Succeeded {
        return text_events.is_empty();
    }
    let Some(result) = candidate.result.as_ref() else {
        return false;
    };
    let Some(message_id) = result.message_id.as_deref() else {
        return false;
    };
    let mut started = false;
    let mut ended = false;
    let mut content = String::new();
    for event in text_events {
        match event["type"].as_str() {
            Some("TEXT_MESSAGE_START") if !started && !ended => {
                if event["messageId"].as_str() != Some(message_id)
                    || event["role"].as_str() != Some("assistant")
                {
                    return false;
                }
                started = true;
            }
            Some("TEXT_MESSAGE_CONTENT") if started && !ended => {
                if event["messageId"].as_str() != Some(message_id) {
                    return false;
                }
                let Some(delta) = event["delta"].as_str() else {
                    return false;
                };
                content.push_str(delta);
            }
            Some("TEXT_MESSAGE_END") if started && !ended => {
                if event["messageId"].as_str() != Some(message_id) {
                    return false;
                }
                ended = true;
            }
            _ => return false,
        }
    }
    started && ended && content == result.text
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeOutputObservation {
    revision: u64,
    bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SequencedEvent {
    sequence: u64,
    event: Value,
}
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ledger {
    epoch: u64,
    grants: BTreeMap<String, Grant>,
    runs: BTreeMap<String, Record>,
    chat_recoveries: BTreeMap<String, recovery::ChatRecoveryRecord>,
}

/// One instance per native app service. Its caller owns the library RuntimeLease.
pub struct AgentStore {
    path: PathBuf,
    events_root: PathBuf,
    // Serialize final publication with task/event reads, cancellation and
    // grant/admission changes. Never hold this lock across an async await.
    protocol_commit: Mutex<()>,
    ledger: Mutex<Ledger>,
    // Any ledger write can fail after rename. Until reopening the store, its
    // old in-memory ledger must not overwrite that uncertain disk.
    storage_uncertain: AtomicBool,
    #[cfg(test)]
    fail_next_persist_after_rename: AtomicBool,
    #[cfg(test)]
    fail_next_event_sync: AtomicBool,
    #[cfg(test)]
    persist_failure_barrier: Mutex<Option<std::sync::Arc<std::sync::Barrier>>>,
}

/// Capability proving that a caller is inside this store's serialized
/// terminal-publication transaction.
pub struct ProtocolCommitPermit<'a> {
    store: &'a AgentStore,
}
/// Result of an atomic native grant update. An explicit revoke is always a
/// change, including when no grant is currently stored for the release.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigureResult {
    pub changed: bool,
    pub affected_run_ids: Vec<String>,
}
/// Newly accepted tasks execute exactly once; duplicates only return a snapshot.
pub struct Admission {
    pub task: Task,
    pub start: bool,
    pub tools: BTreeMap<String, ToolGrant>,
    pub skills: Vec<String>,
    pub chat_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolEventPage {
    pub events: Vec<Value>,
    pub sequence: u64,
}

fn invalid(message: &str) -> AppError {
    AppError::Invalid(message.into())
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn digest(input: &Input, chat_id: Option<&str>) -> Result<String, AppError> {
    // The product enables serde_json/preserve_order. A schema reconstructed
    // after reload can have another object-key order without changing input.
    fn canonical(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<_, _> = map
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
            other => other,
        }
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            canonical(serde_json::to_value(input)?),
            chat_id,
        ))?)
    ))
}
/// Validate the bounded private DTO before a native adapter executes it.
/// Public browser APIs accept File objects; only native bridges handle this
/// already-normalized base64 form.
pub fn validate_input(input: &Input) -> Result<(), AppError> {
    let content_attachment_count = input
        .content_parts
        .iter()
        .filter(|part| matches!(part, InputContentPart::Attachment { .. }))
        .count();
    if input.request_id.is_empty()
        || input.request_id.len() > 128
        || input.request_id.chars().any(char::is_control)
        || input.protocol_thread_id.as_ref().is_some_and(|thread| {
            thread.is_empty() || thread.len() > 128 || thread.chars().any(char::is_control)
        })
        || input.protocol_parent_run_id.as_ref().is_some_and(|run| {
            run.is_empty() || run.len() > 128 || run.chars().any(char::is_control)
        })
        || input.protocol_input_digest.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || input.protocol_message_sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || input.protocol_history_digest.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || (input.prompt.trim().is_empty()
            && input.attachments.is_empty()
            && content_attachment_count == 0)
        || input.prompt.len() > 256 * 1024
        || input.attachments.len() + content_attachment_count > MAX_ATTACHMENTS
        || input.content_parts.len() > 256
        || input.tools.len() > 128
        || input.skills.len() > 128
        || input.skills.iter().any(|skill| !valid_skill_id(skill))
        || input
            .skills
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != input.skills.len()
        || input
            .output_schema
            .as_ref()
            .is_some_and(|schema| !schema.is_object() || schema.to_string().len() > 256 * 1024)
    {
        return Err(invalid("invalid Agent input"));
    }
    let mut total = 0usize;
    let mut account = |name: &str, media_type: &str, encoded: &str| {
        if name.trim().is_empty()
            || name.len() > 255
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
            || media_type.trim().is_empty()
            || media_type.len() > 127
            || media_type.chars().any(char::is_control)
            || encoded.len() > MAX_ATTACHMENT_BASE64_BYTES
            || !valid_base64(encoded)
        {
            return Err(invalid("invalid Agent attachments"));
        }
        let padding = encoded
            .as_bytes()
            .iter()
            .rev()
            .take_while(|byte| **byte == b'=')
            .count();
        let estimated = encoded
            .len()
            .saturating_div(4)
            .saturating_mul(3)
            .saturating_sub(padding);
        total = total.saturating_add(estimated);
        if estimated > MAX_ATTACHMENT_BYTES || total > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err(invalid("invalid Agent attachments"));
        }
        Ok(())
    };
    for attachment in &input.attachments {
        account(&attachment.name, &attachment.media_type, &attachment.base64)?;
    }
    for part in &input.content_parts {
        match part {
            InputContentPart::Text { text } if text.len() <= 256 * 1024 => {}
            InputContentPart::Text { .. } => return Err(invalid("invalid Agent input")),
            InputContentPart::Attachment {
                name,
                media_type,
                base64,
            } => account(name, media_type, base64)?,
        }
    }
    Ok(())
}
fn valid_base64(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/'))
        && bytes[bytes.len() - padding..]
            .iter()
            .all(|byte| *byte == b'=')
}
fn valid_skill_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
/// Stable page-facing errors; storage details and raw OS errors stay in the host.
pub fn admission_error_code(error: &AppError) -> &'static str {
    match error {
        AppError::Busy(_) => "busy",
        AppError::Invalid(message) => match message.as_str() {
            "Agent is not authorized for this release" => "agent_not_authorized",
            "tool is not declared and granted" => "tool_not_authorized",
            "Skill is not declared and granted" => "skill_not_authorized",
            "host wake required" => "host_wake_required",
            "invalid Agent attachments" => "invalid_attachments",
            "requestId conflicts with an earlier request" => "request_conflict",
            "requestId belongs to an earlier invocation" => "stale_request",
            "requested limits exceed host limits" => "limits_exceeded",
            "Agent storage unavailable" | "Agent grant generation exhausted" => {
                "agent_storage_unavailable"
            }
            _ => "invalid_agent_input",
        },
        _ => "agent_storage_unavailable",
    }
}

/// Public task failures provide actionable text without provider/credential details.
pub fn error_message(code: &str) -> &'static str {
    match code {
        "agent_not_authorized" => {
            "Enable Agent access and select a model in this application's native settings."
        }
        "tool_not_authorized" => {
            "The requested tool must be declared by the application and approved in native Agent settings."
        }
        "skill_not_authorized" => {
            "The requested Skill must be included in this release and enabled in native Agent settings."
        }
        "host_wake_required" => {
            "The native host must acquire and bind a background execution lease before this request can start."
        }
        "busy" => {
            "This application already has an active Agent task. Wait for it or cancel it first."
        }
        "request_conflict" => {
            "This request ID was already used with different input. Retry the original input or use a new request ID."
        }
        "stale_request" => {
            "This request belongs to an earlier application invocation and cannot be started again."
        }
        "limits_exceeded" => {
            "Requested task limits exceed the limits selected in native Agent settings."
        }
        "invalid_agent_input" => {
            "The Agent request is invalid. Check its prompt, attachments, schema and limits."
        }
        "model_unavailable" | "model_credentials_unavailable" => {
            "Configure an available model in this application's native Agent settings."
        }
        "model_vision_unsupported" => {
            "The selected model does not support the supplied attachments. Select a compatible model in native Agent settings."
        }
        "model_tools_unsupported" => {
            "The selected model does not support the tools required for this task or structured result."
        }
        "model_structured_output_unsupported" => {
            "The selected model route cannot enforce the requested structured result schema. Choose a model with compatible structured-output support."
        }
        "chat_structured_output_unsupported" => {
            "Structured results cannot be safely committed to a persistent App Chat yet. Remove the result schema or use a stateless Agent request."
        }
        "model_invalid_request" => {
            "The model request was invalid. Check its input, attachment and structured-output requirements."
        }
        "model_provider_http_400" => {
            "The model provider rejected the request with HTTP 400. Check the selected model's request format and attachment support."
        }
        "model_auth_failed" | "model_provider_http_401" => {
            "The model provider rejected authentication. Check the provider connection in trusted native settings."
        }
        "model_provider_http_403" => {
            "The model provider denied access to this request. Check account and model access."
        }
        "model_provider_http_413" | "model_request_too_large" => {
            "The request exceeds the provider's size or context limit. Use smaller files or a shorter request."
        }
        "model_provider_http_422" => {
            "The model provider rejected the request schema or format. Check the structured-output schema."
        }
        "model_rate_limited" | "model_provider_http_429" => {
            "The model provider rate-limited this request. Wait a while and try again."
        }
        "model_quota_exceeded" => {
            "The model provider account has reached a usage or billing limit."
        }
        "model_provider_http_5xx" | "model_provider_failed" => {
            "The model provider could not complete the request. Try again later."
        }
        "model_transport_failed" => {
            "The connection to the model provider failed. Check network, proxy and TLS settings."
        }
        "model_invalid_response" => {
            "The model provider returned a response the host could not decode. Try again or select another model."
        }
        "model_request_not_sent" => {
            "The host rejected the model request before sending it. Review model settings and retry."
        }
        "model_request_outcome_unknown" => {
            "An earlier model request may have been sent. Check task and provider state before retrying to avoid duplicate work or charges."
        }
        "model_request_attempts_exhausted" => {
            "The model request attempt limit was reached. Review task history and usage before starting another run."
        }
        "model_capability_unsupported" => {
            "A required model capability is unavailable on the selected route. Choose a compatible model in native settings."
        }
        "model_media_unavailable" => {
            "The host could not prepare or process this media with the selected model. Check the file or choose a compatible model."
        }
        "model_media_partial" => {
            "Only part of the media analysis completed, so no result was accepted. Review usage before manually starting another run."
        }
        "model_settings_failed" => {
            "The host could not resolve the selected model settings. Reopen native Agent settings or try again later."
        }
        "model_cost_unavailable" => {
            "The host could not finalize model usage accounting. Check this task's status before retrying."
        }
        "model_file_upload_uncertain" => {
            "The provider upload outcome is uncertain. Verify task and provider state before retrying; an automatic retry could create a duplicate upload."
        }
        "invalid_images" | "invalid_attachments" => {
            "A file could not be decoded or exceeds the attachment limits. Choose a smaller valid file."
        }
        "attachment_type_unsupported" => {
            "This file type is not supported. Use a text document, PDF or supported image format."
        }
        "skill_unavailable" | "skill_integrity_failed" => {
            "A selected release Skill file is unavailable or failed integrity verification. Publish a new release or choose another Skill."
        }
        "skill_scope_limit_exceeded" => {
            "Too much Skill data is retained by active or unfinished tasks. Wait for those tasks and cleanup to finish, or select fewer or smaller Skills, before starting another request."
        }
        "invalid_output_schema" | "unsupported_schema_dialect" => {
            "The result schema must be a valid Draft-7 object with local references only."
        }
        "structured_output_missing" | "invalid_output" => {
            "The model did not return a result that matches the requested schema."
        }
        "timeout_exceeded" => "The Agent task reached its time limit and was stopped.",
        "cancelled" | "authorization_revoked" => {
            "The Agent task was cancelled or its authorization was revoked."
        }
        IOS_BACKGROUND_EXPIRED => {
            "iOS ended the background execution time for this Agent task. Check the task and any external effects before retrying."
        }
        "mcp_definition_changed" => {
            "The MCP configuration or tool definition changed. Review and authorize the current tool again."
        }
        "tool_cleanup_incomplete" => {
            "A tool has not finished stopping. The host is retaining the task's resources until cleanup completes."
        }
        "chat_deferred_tools_pending" => {
            "This Chat contains an unfinished tool operation. Review its effects before continuing; it will not be replayed automatically. Earlier Chat history remains available."
        }
        "chat_context_unavailable" => {
            "The host could not prepare a verified model context for this Chat. Reopen the Chat and review its history before retrying."
        }
        "chat_context_compaction_unsupported" => {
            "This Chat cannot safely shorten the history required for this request. Reduce the new input, choose a model with a larger context limit, or start a new Chat."
        }
        "chat_context_limit_exceeded" => {
            "This Chat request exceeds its available model context limit. Use a smaller request or fewer attachments, choose a model with a larger context limit, or start a new Chat."
        }
        "chat_context_budget_unknown" => {
            "The host could not determine the selected model's context limit. Check the model configuration in native Agent settings or choose another model."
        }
        "chat_context_authority_revoked" => {
            "This Chat request was cancelled or its authorization changed. Review its status and any tool effects, then check native Agent permissions before starting another request."
        }
        "chat_tool_unsupported" => {
            "The requested tool is not supported in persistent App Chats. Choose another tool or start an independent Agent request."
        }
        "chat_transcript_external_result_unmanaged" => {
            "This Chat contains a tool result or media reference the host cannot safely restore. Review the task and saved history before choosing another tool or starting a new Chat."
        }
        "local_app_chat_inline_result_too_large" => {
            "A tool returned more data than this Chat can retain. Request a smaller result or fewer files, and review the tool's effects before starting another run."
        }
        "chat_transcript_unavailable" => {
            "The Chat history could not be verified. Check the host status and saved data before continuing. The task will not be replayed automatically."
        }
        "agent_storage_unavailable" | "task_storage_failed" => {
            "Agent task storage is unavailable. Existing task data has been preserved."
        }
        PROTOCOL_EVENT_STORAGE_FAILED => {
            "The Agent event stream could not be verified. Review this task before retrying."
        }
        PROTOCOL_COMMIT_INCOMPLETE => {
            "The Agent result commit was interrupted. Existing task and Chat data have been preserved; review them before continuing."
        }
        PROTOCOL_TERMINAL_CONFLICT => {
            "The saved Agent task and its terminal event disagree. Review this task before retrying."
        }
        _ => "The Agent task could not complete. Review the host status before retrying.",
    }
}

impl AgentStore {
    pub fn open(root: &Path) -> Result<Self, AppError> {
        Self::open_with_event_validator(root, protocol::validate_event)
    }

    fn open_with_event_validator(
        root: &Path,
        validate_event: impl Fn(&Value) -> Result<(), protocol::ProtocolError>,
    ) -> Result<Self, AppError> {
        ensure_private_directory(root)?;
        let events_root = root.join("protocol-events");
        ensure_private_directory(&events_root)?;
        let path = root.join("agent-tasks.json");
        let mut ledger: Ledger = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing authority is not an empty store when event evidence
                // or an interrupted ledger write remains. Never overwrite it.
                if fs::read_dir(&events_root)?.next().transpose()?.is_some()
                    || fs::read_dir(root)?.any(|entry| {
                        entry.map_or(true, |entry| {
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
                            name.starts_with("agent-tasks.") && name.ends_with(".tmp")
                        })
                    })
                {
                    return Err(Self::storage_unavailable());
                }
                Ledger::default()
            }
            Err(e) => return Err(e.into()),
        };
        if ledger
            .runs
            .values()
            .any(|record| record.protocol_thread_id.is_some())
        {
            // Check the validator independently of any one event file. A
            // broken schema must not be persisted as per-run corruption.
            validate_event(&json!({
                "type":"RUN_STARTED", "threadId":"schema-probe", "runId":"schema-probe"
            }))
            .map_err(|_| protocol_schema_unavailable())?;
        }
        recovery::validate_records(&ledger)?;
        let mut pending_terminal_events = Vec::new();
        for record in ledger.runs.values_mut() {
            if let Some(candidate) = record.protocol_commit_pending.take() {
                // Even a complete RUN_FINISHED cannot authorize success while
                // the ledger still records an unfinished publication. Keep the
                // candidate and original event bytes for explicit inspection.
                record.protocol_candidate_evidence = Some(candidate.clone());
                record.task = pending_commit_task(&candidate);
                continue;
            }
            let events = if record.protocol_thread_id.is_some() {
                // An interrupted write is evidence, not a stream we can
                // repair by appending to it. Keep the original bytes in place
                // and isolate this run from the rest of the app.
                if protocol_storage_failed(record) {
                    if record.task.state != State::Interrupted || record.task.result.is_some() {
                        let code = record
                            .task
                            .error
                            .as_ref()
                            .expect("protocol failure")
                            .code
                            .clone();
                        mark_protocol_failure(record, &code);
                    }
                    continue;
                }
                let event_path = protocol_event_path(&events_root, &record.task.run_id)?;
                let thread_id = record.protocol_thread_id.as_deref().expect("protocol run");
                match read_protocol_events_with_validator(
                    &event_path,
                    Some((thread_id, &record.task.request_id)),
                    &validate_event,
                ) {
                    Ok(events) if !events.is_empty() => Some(events),
                    Ok(_) | Err(EventLogReadError::Corrupt(_)) => {
                        mark_protocol_storage_failed(record);
                        continue;
                    }
                    Err(EventLogReadError::Unavailable(error)) => return Err(error),
                }
            } else {
                None
            };
            if !record.task.state.terminal() {
                record.task.state = State::Interrupted;
                record.task.phase = None;
                record.task.error = if record.stop_intent == Some(StopIntent::IosBackgroundExpired)
                {
                    Some(TaskError {
                        code: IOS_BACKGROUND_EXPIRED.into(),
                        message: error_message(IOS_BACKGROUND_EXPIRED).into(),
                    })
                } else {
                    Some(TaskError {
                        code: "host_restarted".into(),
                        message: "The host restarted. This task was interrupted and will not be replayed.".into(),
                    })
                };
                record.task.sequence += 1;
                record.task.updated_at = now();
            } else if events.as_ref().is_some_and(|events| {
                !events
                    .last()
                    .is_some_and(|entry| is_terminal_event(&entry.event))
            }) && record.task.state == State::Succeeded
            {
                if record.task.result.is_some() {
                    record.protocol_candidate_evidence = Some(record.task.clone());
                }
                record.task.state = State::Interrupted;
                record.task.error = Some(TaskError {
                    code: "protocol_terminal_missing".into(),
                    message: "The run completed without a durable terminal protocol event.".into(),
                });
                record.task.result = None;
                record.task.sequence += 1;
                record.task.updated_at = now();
            }
            if events
                .as_ref()
                .and_then(|events| events.last())
                .is_some_and(|entry| {
                    is_terminal_event(&entry.event) && !terminal_matches_task(record, &entry.event)
                })
            {
                mark_protocol_failure(record, PROTOCOL_TERMINAL_CONFLICT);
                continue;
            }
            if let Some(events) = events.as_ref().filter(|events| {
                events
                    .last()
                    .is_some_and(|entry| is_terminal_event(&entry.event))
            }) {
                let mut committed_record = record.clone();
                committed_record.protocol_commit_pending = Some(record.task.clone());
                if !complete_protocol_commit_matches(&committed_record, events) {
                    mark_protocol_failure(record, PROTOCOL_TERMINAL_CONFLICT);
                    continue;
                }
            }
            if events.as_ref().is_some_and(|events| {
                !events
                    .last()
                    .is_some_and(|entry| is_terminal_event(&entry.event))
            }) {
                pending_terminal_events.push((
                    record.owner.clone(),
                    record.task.run_id.clone(),
                    record.task.request_id.clone(),
                    record.protocol_thread_id.clone().expect("protocol run"),
                    record.task.state.clone(),
                    record.task.error.clone(),
                ));
            }
        }
        let store = Self {
            path,
            events_root,
            protocol_commit: Mutex::new(()),
            ledger: Mutex::new(ledger),
            storage_uncertain: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_persist_after_rename: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_event_sync: AtomicBool::new(false),
            #[cfg(test)]
            persist_failure_barrier: Mutex::new(None),
        };
        for (owner, task_id, run_id, thread_id, state, task_error) in pending_terminal_events {
            let terminal = match state {
                State::Cancelled => json!({
                    "type":"RUN_FINISHED",
                    "threadId":thread_id,
                    "runId":run_id,
                    "outcome":{"type":"cancelled"}
                }),
                State::Failed => json!({
                    "type":"RUN_ERROR",
                    "message":task_error.as_ref().map_or("The Agent run failed.", |error| error.message.as_str()),
                    "code":task_error.as_ref().map_or("agent_failed", |error| error.code.as_str())
                }),
                State::Succeeded | State::Interrupted | State::Queued | State::Running => {
                    json!({
                        "type":"RUN_ERROR",
                        "message":task_error.as_ref().map_or("The host restarted before the Agent run completed.", |error| error.message.as_str()),
                        "code":task_error.as_ref().map_or("host_restarted", |error| error.code.as_str())
                    })
                }
            };
            // If recovery cannot write this event, no ledger write has been
            // attempted. Earlier event appends may have succeeded. A partial
            // event remains for the next open to validate and, if damaged,
            // isolate; a complete write may be indistinguishable from a sync.
            store.append_event(&owner, &task_id, terminal)?;
        }
        {
            let ledger = store
                .ledger
                .lock()
                .map_err(|_| invalid("Agent storage unavailable"))?;
            store.persist(&ledger)?;
        }
        Ok(store)
    }
    fn persist(&self, ledger: &Ledger) -> Result<(), AppError> {
        let temp = self
            .path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let write = (|| -> Result<(), AppError> {
            use std::io::Write;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&serde_json::to_vec(ledger)?)?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            #[cfg(test)]
            if self
                .fail_next_persist_after_rename
                .swap(false, Ordering::AcqRel)
            {
                if let Some(barrier) = self.persist_failure_barrier.lock().unwrap().clone() {
                    barrier.wait();
                    barrier.wait();
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected failure after ledger rename",
                )
                .into());
            }
            fs::File::open(
                self.path
                    .parent()
                    .ok_or_else(|| invalid("Agent storage unavailable"))?,
            )?
            .sync_all()?;
            Ok(())
        })();
        if write.is_err() {
            self.quarantine_storage();
            let _ = fs::remove_file(temp);
        }
        write
    }
    /// Fence this in-memory authority immediately after any uncertain write.
    /// Recovery must open a new store; another best-effort write is not a fence.
    pub fn quarantine_storage(&self) {
        self.storage_uncertain.store(true, Ordering::Release);
    }

    fn storage_unavailable() -> AppError {
        AppError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Agent storage state is uncertain; reopen the store",
        ))
    }

    fn reject_pending_commit(record: &Record) -> Result<(), AppError> {
        if record.protocol_commit_pending.is_some() {
            Err(Self::storage_unavailable())
        } else {
            Ok(())
        }
    }

    fn lock_ledger(&self) -> Result<MutexGuard<'_, Ledger>, AppError> {
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        if self.storage_uncertain.load(Ordering::Acquire) {
            return Err(Self::storage_unavailable());
        }
        Ok(ledger)
    }

    fn change<T>(
        &self,
        edit: impl FnOnce(&mut Ledger) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut ledger = self.lock_ledger()?;
        let mut next = ledger.clone();
        let result = edit(&mut next)?;
        self.persist(&next)?;
        *ledger = next;
        Ok(result)
    }
    /// Serialize a synchronous host finalization with readers and cancellation.
    /// The closure may call update/append_event, but must not call methods
    /// which acquire this same guard or perform any async work.
    pub fn with_protocol_commit<T>(&self, commit: impl FnOnce() -> T) -> Result<T, AppError> {
        let _guard = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        if self.storage_uncertain.load(Ordering::Acquire) {
            return Err(Self::storage_unavailable());
        }
        Ok(commit())
    }

    /// Run terminal publication with an unforgeable, store-scoped capability.
    /// The permit cannot outlive this synchronous closure.
    pub fn with_protocol_commit_permit<T>(
        &self,
        commit: impl FnOnce(&ProtocolCommitPermit<'_>) -> T,
    ) -> Result<T, AppError> {
        let _guard = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        if self.storage_uncertain.load(Ordering::Acquire) {
            return Err(Self::storage_unavailable());
        }
        let permit = ProtocolCommitPermit { store: self };
        Ok(commit(&permit))
    }

    fn validate_protocol_commit_permit(
        &self,
        permit: &ProtocolCommitPermit<'_>,
    ) -> Result<(), AppError> {
        if std::ptr::eq(self, permit.store) {
            Ok(())
        } else {
            Err(invalid("invalid Agent terminal commit permit"))
        }
    }

    /// Persist the terminal candidate before any result Chat or AG-UI event is
    /// written. While this intent exists, public task and event reads fail
    /// closed and the result remains absent from Chat history.
    pub fn prepare_protocol_terminal(
        &self,
        permit: &ProtocolCommitPermit<'_>,
        owner: &Owner,
        id: &str,
        state: State,
        result: Option<TaskResult>,
        usage: Option<Value>,
        error: Option<String>,
    ) -> Result<Task, AppError> {
        self.validate_protocol_commit_permit(permit)?;
        owner.validate()?;
        if !state.terminal() {
            return Err(invalid("invalid Agent terminal state"));
        }
        self.change(|ledger| {
            let authorized = ledger.runs.get(id).is_some_and(|record| {
                !record.cancel_requested
                    && ledger.grants.get(&owner.app_id).is_some_and(|grant| {
                        grant.epoch == record.epoch
                            && grant.owner.release_digest == owner.release_digest
                    })
            });
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|record| record.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            Self::reject_pending_commit(record)?;
            if record.protocol_thread_id.is_none() {
                return Err(invalid("Agent task is not an AG-UI run"));
            }
            if record.task.state.terminal() {
                return Err(invalid("Agent task already ended"));
            }
            let candidate = terminal_candidate(record, authorized, state, result, usage, error)?;
            record.protocol_commit_pending = Some(candidate.clone());
            record.task = pending_commit_task(&candidate);
            Ok(candidate)
        })
    }

    /// Publish the already persisted terminal candidate only after its Chat
    /// record and complete event stream have been durably written.
    pub fn finalize_protocol_terminal(
        &self,
        permit: &ProtocolCommitPermit<'_>,
        owner: &Owner,
        id: &str,
    ) -> Result<Task, AppError> {
        self.validate_protocol_commit_permit(permit)?;
        owner.validate()?;
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|record| record.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            let candidate = record
                .protocol_commit_pending
                .clone()
                .ok_or_else(|| invalid("Agent task has no pending terminal commit"))?;
            if !candidate.state.terminal()
                || record.task.state != State::Interrupted
                || record.task.error.as_ref().map(|error| error.code.as_str())
                    != Some(PROTOCOL_COMMIT_INCOMPLETE)
            {
                return Err(Self::storage_unavailable());
            }
            let thread_id = record
                .protocol_thread_id
                .as_deref()
                .ok_or_else(|| invalid("Agent task is not an AG-UI run"))?;
            let event_path = self.event_path(id)?;
            let events =
                read_protocol_events(&event_path, Some((thread_id, &record.task.request_id)))?;
            if !complete_protocol_commit_matches(record, &events) {
                return Err(invalid("AG-UI terminal event does not match pending task"));
            }
            record.task = candidate.clone();
            record.protocol_commit_pending = None;
            Ok(candidate)
        })
    }

    /// Return only task-ledger-authorized assistant messages for Chat startup
    /// visibility. A pending candidate is intentionally never returned.
    pub fn committed_protocol_assistants(
        &self,
    ) -> Result<BTreeMap<String, (String, String, String)>, AppError> {
        let ledger = self.lock_ledger()?;
        Ok(ledger
            .runs
            .values()
            .filter_map(|record| {
                if record.protocol_commit_pending.is_some()
                    || record.protocol_thread_id.is_none()
                    || record.task.state != State::Succeeded
                    || protocol_storage_failed(record)
                {
                    return None;
                }
                let chat_id = record.chat_id.as_ref()?;
                let result = record.task.result.as_ref()?;
                let message_id = result.message_id.as_ref()?;
                Some((
                    record.task.run_id.clone(),
                    (chat_id.clone(), message_id.clone(), result.text.clone()),
                ))
            })
            .collect())
    }

    /// Trusted native settings UI only. A changed grant invalidates every task
    /// lease immediately; the executor must cancel and reap returned run IDs.
    pub fn configure(
        &self,
        owner: Owner,
        settings: Option<Settings>,
    ) -> Result<Vec<String>, AppError> {
        Ok(self
            .configure_with_change(owner, settings)?
            .affected_run_ids)
    }

    /// Return whether the grant was changed as part of the same locked update.
    /// Explicit revocation always counts as a change.
    pub fn configure_with_change(
        &self,
        owner: Owner,
        settings: Option<Settings>,
    ) -> Result<ConfigureResult, AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        owner.validate()?;
        if let Some(s) = &settings {
            s.validate()?;
        }
        let mut ledger = self.lock_ledger()?;
        if let Some(requested) = &settings {
            if ledger.grants.get(&owner.app_id).is_some_and(|grant| {
                grant.owner.release_digest == owner.release_digest && grant.settings == *requested
            }) {
                return Ok(ConfigureResult {
                    changed: false,
                    affected_run_ids: Vec::new(),
                });
            }
        }

        let mut next = ledger.clone();
        next.epoch = next
            .epoch
            .checked_add(1)
            .ok_or_else(|| invalid("Agent grant generation exhausted"))?;
        let epoch = next.epoch;
        match settings {
            Some(settings) => {
                next.grants.insert(
                    owner.app_id.clone(),
                    Grant {
                        owner: owner.clone(),
                        settings,
                        epoch,
                    },
                );
            }
            None => {
                next.grants.remove(&owner.app_id);
            }
        }
        let affected = next
            .runs
            .values()
            .filter(|r| r.owner.app_id == owner.app_id && !r.task.state.terminal())
            .map(|r| r.task.run_id.clone())
            .collect();
        self.persist(&next)?;
        *ledger = next;
        Ok(ConfigureResult {
            changed: true,
            affected_run_ids: affected,
        })
    }
    pub fn settings(&self, owner: &Owner) -> Result<Option<Settings>, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        Ok(ledger
            .grants
            .get(&owner.app_id)
            .filter(|g| g.owner.release_digest == owner.release_digest)
            .map(|g| g.settings.clone()))
    }
    pub fn admit(
        &self,
        owner: &Owner,
        input: &Input,
        declared_tools: &[String],
    ) -> Result<Admission, AppError> {
        self.admit_scoped(owner, input, declared_tools, &[], None, true)
    }

    /// Admit one App task after the host has intersected its release, settings,
    /// platform and enterprise policies. `allow_new` is the iOS native wake
    /// reservation gate; a durable duplicate is resolved before that gate.
    pub fn admit_scoped(
        &self,
        owner: &Owner,
        input: &Input,
        declared_tools: &[String],
        declared_skills: &[String],
        chat_id: Option<&str>,
        allow_new: bool,
    ) -> Result<Admission, AppError> {
        self.admit_scoped_with_check(
            owner,
            input,
            declared_tools,
            declared_skills,
            chat_id,
            allow_new,
            || Ok(()),
        )
    }

    /// Perform a host-owned, synchronous history check under the same lock as
    /// admission. The check must not acquire the protocol commit lock again.
    pub fn admit_scoped_with_check(
        &self,
        owner: &Owner,
        input: &Input,
        declared_tools: &[String],
        declared_skills: &[String],
        chat_id: Option<&str>,
        allow_new: bool,
        check: impl FnOnce() -> Result<(), AppError>,
    ) -> Result<Admission, AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        if self.storage_uncertain.load(Ordering::Acquire) {
            return Err(Self::storage_unavailable());
        }
        owner.validate()?;
        validate_input(input)?;
        if chat_id
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        {
            return Err(invalid("invalid Chat ID"));
        }
        let hash = digest(input, chat_id)?;
        self.change(move |ledger| {
            let grant = ledger
                .grants
                .get(&owner.app_id)
                .filter(|g| g.owner.release_digest == owner.release_digest)
                .ok_or_else(|| invalid("Agent is not authorized for this release"))?;
            let mut tools = BTreeMap::new();
            for id in &input.tools {
                if !declared_tools.contains(id) {
                    return Err(invalid("tool is not declared and granted"));
                }
                tools.insert(
                    id.clone(),
                    grant
                        .settings
                        .tools
                        .get(id)
                        .ok_or_else(|| invalid("tool is not declared and granted"))?
                        .clone(),
                );
            }
            let mut skills = Vec::new();
            for id in &input.skills {
                if !declared_skills.contains(id) || !grant.settings.skills.contains(id) {
                    return Err(invalid("Skill is not declared and granted"));
                }
                skills.push(id.clone());
            }
            if let Some(record) = ledger.runs.values().find(|r| {
                r.owner.app_id == owner.app_id
                    && r.owner.release_digest == owner.release_digest
                    && r.task.request_id == input.request_id
            }) {
                Self::reject_pending_commit(record)?;
                if record.input_digest != hash {
                    return Err(invalid("requestId conflicts with an earlier request"));
                }
                return Ok(Admission {
                    task: record.task.clone(),
                    start: false,
                    tools,
                    skills,
                    chat_id: record.chat_id.clone(),
                });
            }
            if chat_id.is_some_and(|chat| {
                ledger.runs.values().any(|record| {
                    record.owner.app_id == owner.app_id
                        && record.chat_id.as_deref() == Some(chat)
                        && record.task.request_id == input.request_id
                        && recovery::archived(ledger, chat, &record.task.run_id)
                })
            }) {
                return Err(invalid("requestId conflicts with an earlier request"));
            }
            check()?;
            if chat_id.is_some_and(|id| recovery::pending(ledger, id)) {
                return Err(Self::storage_unavailable());
            }
            if chat_id.is_some_and(|chat_id| {
                ledger.runs.values().any(|record| {
                    record.chat_id.as_deref() == Some(chat_id)
                        && !recovery::archived(ledger, chat_id, &record.task.run_id)
                        && (record.protocol_commit_pending.is_some()
                            || protocol_storage_failed(record)
                            || matches!(
                                record.task.state,
                                State::Failed | State::Cancelled | State::Interrupted
                            ))
                })
            }) {
                return Err(invalid("Chat has an incomplete or unverified turn"));
            }
            if !allow_new {
                return Err(invalid("host wake required"));
            }
            if ledger
                .runs
                .values()
                .any(|r| r.owner.app_id == owner.app_id && !r.task.state.terminal())
            {
                return Err(AppError::Busy(owner.app_id.clone()));
            }
            let turns = input.max_turns.unwrap_or(grant.settings.max_turns);
            let timeout = input.timeout_ms.unwrap_or(grant.settings.timeout_ms);
            if turns == 0
                || turns > grant.settings.max_turns
                || timeout == 0
                || timeout > grant.settings.timeout_ms
            {
                return Err(invalid("requested limits exceed host limits"));
            }
            let timestamp = now();
            let task = Task {
                run_id: uuid::Uuid::new_v4().to_string(),
                request_id: input.request_id.clone(),
                state: State::Queued,
                sequence: 1,
                phase: None,
                progress: None,
                created_at: timestamp,
                updated_at: timestamp,
                model: grant.settings.model.clone(),
                limits: Limits {
                    max_turns: turns,
                    timeout_ms: timeout,
                },
                result: None,
                usage: None,
                error: None,
            };
            ledger.runs.insert(
                task.run_id.clone(),
                Record {
                    owner: owner.clone(),
                    input_digest: hash,
                    epoch: grant.epoch,
                    cancel_requested: false,
                    stop_intent: None,
                    chat_id: chat_id.map(str::to_owned),
                    protocol_thread_id: input.protocol_thread_id.clone(),
                    protocol_input_digest: input.protocol_input_digest.clone(),
                    chat_transcript_checkpoint: None,
                    native_output: None,
                    protocol_commit_pending: None,
                    protocol_candidate_evidence: None,
                    task: task.clone(),
                },
            );
            Ok(Admission {
                task,
                start: true,
                tools,
                skills,
                chat_id: chat_id.map(str::to_owned),
            })
        })
    }
    pub fn get(&self, owner: &Owner, id: &str) -> Result<Task, AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .get(id)
            .filter(|r| {
                r.owner.app_id == owner.app_id && r.owner.release_digest == owner.release_digest
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        Self::reject_pending_commit(record)?;
        Ok(record.task.clone())
    }
    /// Resolve the public AG-UI runId to the private, path-safe task UUID.
    pub fn task_id_for_request(&self, owner: &Owner, run_id: &str) -> Result<String, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        Self::reject_pending_commit(record)?;
        Ok(record.task.run_id.clone())
    }
    /// Resolve the private Chat binding for a current-release AG-UI request.
    /// This intentionally does not add the field to the public Task DTO.
    pub fn protocol_chat_id_for_request(
        &self,
        owner: &Owner,
        run_id: &str,
    ) -> Result<Option<String>, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        Self::reject_pending_commit(record)?;
        Ok(record.chat_id.clone())
    }

    /// Persist the exact strict-writer capture before terminal publication.
    pub fn record_chat_transcript_checkpoint(
        &self,
        owner: &Owner,
        id: &str,
        chat_id: &str,
        checkpoint: ChatTranscriptCheckpoint,
    ) -> Result<(), AppError> {
        owner.validate()?;
        if chat_id.is_empty()
            || chat_id.len() > 128
            || checkpoint.byte_len == 0
            || !valid_hex(&checkpoint.sha256, 64)
        {
            return Err(invalid("invalid Chat transcript checkpoint"));
        }
        self.change(|ledger| {
            let previous_length = {
                let record = ledger
                    .runs
                    .get(id)
                    .filter(|record| record.owner == *owner)
                    .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
                Self::reject_pending_commit(record)?;
                if record.task.state != State::Running
                    || record.chat_id.as_deref() != Some(chat_id)
                    || record.protocol_thread_id.as_deref() != Some(chat_id)
                {
                    return Err(invalid(
                        "Chat transcript checkpoint has no active Chat task",
                    ));
                }
                let mut previous_length = 0;
                for previous in ledger.runs.values().filter(|previous| {
                    previous.task.run_id != id
                        && previous.owner.app_id == owner.app_id
                        && previous.chat_id.as_deref() == Some(chat_id)
                        && previous.protocol_thread_id.as_deref() == Some(chat_id)
                        && !recovery::archived(ledger, chat_id, &previous.task.run_id)
                }) {
                    if previous.task.state != State::Succeeded {
                        return Err(Self::storage_unavailable());
                    }
                    let previous_checkpoint = previous
                        .chat_transcript_checkpoint
                        .as_ref()
                        .filter(|checkpoint| {
                            checkpoint.byte_len > 0 && valid_hex(&checkpoint.sha256, 64)
                        })
                        .ok_or_else(Self::storage_unavailable)?;
                    previous_length = previous_length.max(previous_checkpoint.byte_len);
                }
                previous_length
            };
            if checkpoint.byte_len <= previous_length {
                return Err(Self::storage_unavailable());
            }
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|record| record.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            record.chat_transcript_checkpoint = Some(checkpoint);
            Ok(())
        })
    }

    /// Find the last committed transcript revision before a new Chat turn.
    /// The current active run is explicitly excluded; any other unverified
    /// turn blocks opening a persistent writer.
    pub fn latest_chat_transcript_checkpoint(
        &self,
        app_id: &str,
        chat_id: &str,
        exclude_task_id: &str,
    ) -> Result<Option<(Task, ChatTranscriptCheckpoint)>, AppError> {
        if !valid_app_id(app_id) || chat_id.is_empty() || chat_id.len() > 128 {
            return Err(invalid("invalid Chat history selector"));
        }
        let latest_id = {
            let ledger = self.lock_ledger()?;
            if recovery::pending(&ledger, chat_id) {
                return Err(Self::storage_unavailable());
            }
            let records = ledger
                .runs
                .values()
                .filter(|record| {
                    record.owner.app_id == app_id
                        && record.chat_id.as_deref() == Some(chat_id)
                        && record.protocol_thread_id.as_deref() == Some(chat_id)
                        && record.task.run_id != exclude_task_id
                        && !recovery::archived(&ledger, chat_id, &record.task.run_id)
                })
                .collect::<Vec<_>>();
            if records
                .iter()
                .any(|record| record.task.state != State::Succeeded)
            {
                return Err(Self::storage_unavailable());
            }
            if records.is_empty() {
                None
            } else {
                let checkpoints = records
                    .iter()
                    .map(|record| {
                        let checkpoint = record
                            .chat_transcript_checkpoint
                            .as_ref()
                            .filter(|checkpoint| {
                                checkpoint.byte_len > 0 && valid_hex(&checkpoint.sha256, 64)
                            })
                            .ok_or_else(Self::storage_unavailable)?;
                        Ok((record.task.run_id.clone(), checkpoint.clone()))
                    })
                    .collect::<Result<Vec<_>, AppError>>()?;
                let latest_length = checkpoints
                    .iter()
                    .map(|(_, checkpoint)| checkpoint.byte_len)
                    .max()
                    .expect("nonempty Chat records have checkpoints");
                let latest = checkpoints
                    .into_iter()
                    .filter(|(_, checkpoint)| checkpoint.byte_len == latest_length)
                    .collect::<Vec<_>>();
                // Strict writers only append to the prior verified transcript,
                // so a later successful Chat has a longer full-file capture.
                // Same-length checkpoints cannot be ordered safely by Task
                // sequence (which is per run) or wall clock; conflicting or
                // ambiguous evidence blocks continuation.
                if latest.len() != 1 {
                    return Err(Self::storage_unavailable());
                }
                latest.into_iter().next().map(|(id, _)| id)
            }
        };
        latest_id
            .map(|id| self.successful_chat_history_task(app_id, chat_id, &id))
            .transpose()
    }

    /// Read an older successful Chat turn for transcript recovery across a
    /// release update. This is deliberately narrower than page task reads:
    /// the internal task must belong to this App and Chat, have a successful
    /// terminal record, and retain a complete matching AG-UI event stream.
    pub fn successful_chat_history_task(
        &self,
        app_id: &str,
        chat_id: &str,
        id: &str,
    ) -> Result<(Task, ChatTranscriptCheckpoint), AppError> {
        if !valid_app_id(app_id)
            || chat_id.is_empty()
            || chat_id.len() > 128
            || id.is_empty()
            || id.len() > 128
        {
            return Err(invalid("invalid Chat history task selector"));
        }
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .get(id)
            .filter(|record| {
                record.owner.app_id == app_id
                    && record.chat_id.as_deref() == Some(chat_id)
                    && record.protocol_thread_id.as_deref() == Some(chat_id)
            })
            .ok_or_else(|| AppError::NotFound("Chat history task".into()))?;
        if record.protocol_commit_pending.is_some()
            || protocol_storage_failed(record)
            || record.task.state != State::Succeeded
            || record.task.result.as_ref().is_none_or(|result| {
                result.chat_id.as_deref() != Some(chat_id) || result.message_id.is_none()
            })
        {
            return Err(Self::storage_unavailable());
        }
        let checkpoint = record
            .chat_transcript_checkpoint
            .clone()
            .filter(|checkpoint| checkpoint.byte_len > 0 && valid_hex(&checkpoint.sha256, 64))
            .ok_or_else(|| invalid("Chat transcript checkpoint is missing"))?;
        let thread_id = record
            .protocol_thread_id
            .as_deref()
            .expect("checked Chat protocol task");
        let path = self.event_path(id)?;
        let events = read_protocol_events(&path, Some((thread_id, &record.task.request_id)))?;
        let mut committed_record = record.clone();
        committed_record.protocol_commit_pending = Some(record.task.clone());
        if !events
            .last()
            .is_some_and(|entry| is_terminal_event(&entry.event))
            || !terminal_matches_task(record, &events.last().expect("checked terminal").event)
            || !complete_protocol_commit_matches(&committed_record, &events)
        {
            self.quarantine_storage();
            return Err(Self::storage_unavailable());
        }
        Ok((record.task.clone(), checkpoint))
    }
    pub fn get_by_request(&self, owner: &Owner, run_id: &str) -> Result<Task, AppError> {
        let id = self.task_id_for_request(owner, run_id)?;
        self.get(owner, &id)
    }
    pub fn protocol_input_fingerprint(
        &self,
        owner: &Owner,
        run_id: &str,
    ) -> Result<String, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("AG-UI request fingerprint".into()))?;
        Self::reject_pending_commit(record)?;
        record
            .protocol_input_digest
            .clone()
            .ok_or_else(|| AppError::NotFound("AG-UI request fingerprint".into()))
    }
    pub fn thread_id_for_request(&self, owner: &Owner, run_id: &str) -> Result<String, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("AG-UI task".into()))?;
        Self::reject_pending_commit(record)?;
        record
            .protocol_thread_id
            .clone()
            .ok_or_else(|| AppError::NotFound("AG-UI task".into()))
    }
    /// Remove an admission that failed before RUN_STARTED. The request ID can
    /// then be retried because the host never accepted execution. Existing
    /// empty event files are left untouched rather than risking loss of data.
    pub fn rollback_queued_admission(&self, owner: &Owner, id: &str) -> Result<(), AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        owner.validate()?;
        let path = self.event_path(id)?;
        let mut ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .get(id)
            .filter(|record| record.owner == *owner)
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        let identity = record
            .protocol_thread_id
            .as_deref()
            .map(|thread_id| (thread_id, record.task.request_id.as_str()));
        if record.task.state != State::Queued || !read_protocol_events(&path, identity)?.is_empty()
        {
            return Err(invalid("Agent admission has already started"));
        }
        let mut next = ledger.clone();
        next.runs.remove(id);
        self.persist(&next)?;
        *ledger = next;
        Ok(())
    }

    /// Record an initial event-write failure for a run that the host has not
    /// started. The caller must not have spawned an executor. The original
    /// event file is never read, deleted or rewritten by this operation.
    pub fn interrupt_unstarted_protocol_admission(
        &self,
        owner: &Owner,
        id: &str,
    ) -> Result<Task, AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        owner.validate()?;
        let mut ledger = self.lock_ledger()?;
        let mut next = ledger.clone();
        let record = next
            .runs
            .get_mut(id)
            .filter(|record| record.owner == *owner && record.protocol_thread_id.is_some())
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        if record.task.state != State::Queued {
            return Err(invalid("Agent admission has already started"));
        }
        mark_protocol_storage_failed(record);
        let task = record.task.clone();
        self.persist(&next)?;
        *ledger = next;
        Ok(task)
    }
    /// Append one schema-validated AG-UI event to the durable task log.
    pub fn append_event(
        &self,
        owner: &Owner,
        task_id: &str,
        event: Value,
    ) -> Result<u64, AppError> {
        self.append_event_impl(owner, task_id, event, false)
    }

    /// Trusted terminal-commit lane. The caller holds `with_protocol_commit`
    /// and may append only text for the persisted candidate and its matching
    /// terminal event.
    pub fn append_protocol_commit_event(
        &self,
        permit: &ProtocolCommitPermit<'_>,
        owner: &Owner,
        task_id: &str,
        event: Value,
    ) -> Result<u64, AppError> {
        self.validate_protocol_commit_permit(permit)?;
        self.append_event_impl(owner, task_id, event, true)
    }

    fn append_event_impl(
        &self,
        owner: &Owner,
        task_id: &str,
        event: Value,
        pending_commit_lane: bool,
    ) -> Result<u64, AppError> {
        owner.validate()?;
        protocol::validate_event(&event).map_err(|error| {
            if error.code == "protocol_schema_unavailable" {
                protocol_schema_unavailable()
            } else {
                invalid("AG-UI event does not match the pinned protocol")
            }
        })?;
        let path = self.event_path(task_id)?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .get(task_id)
            .filter(|record| record.owner == *owner)
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        if record.protocol_commit_pending.is_some() != pending_commit_lane {
            return Err(Self::storage_unavailable());
        }
        if pending_commit_lane {
            validate_pending_commit_event(record, &event)?;
        } else if protocol_storage_failed(record) {
            return Err(invalid("AG-UI event log is unavailable"));
        }
        let thread_id = record
            .protocol_thread_id
            .as_deref()
            .ok_or_else(|| invalid("Agent task is not an AG-UI run"))?;
        validate_event_identity(&event, thread_id, &record.task.request_id)?;
        let events = read_protocol_events(&path, Some((thread_id, &record.task.request_id)))?;
        if events.is_empty() {
            if event["type"] != "RUN_STARTED" {
                return Err(invalid("AG-UI run must begin with RUN_STARTED"));
            }
        } else {
            if event["type"] == "RUN_STARTED" {
                return Err(invalid("AG-UI run already has RUN_STARTED"));
            }
            if events
                .last()
                .is_some_and(|entry| is_terminal_event(&entry.event))
            {
                return Err(invalid("AG-UI run already has a terminal event"));
            }
        }
        let sequence = events.len() as u64 + 1;
        let entry = SequencedEvent { sequence, event };
        let encoded = serde_json::to_vec(&entry)?;
        if encoded.len() > 3 * 1024 * 1024
            || events.len() >= 10_000
            || fs::metadata(&path)
                .map_or(0, |metadata| metadata.len())
                .saturating_add(encoded.len() as u64 + 1)
                > MAX_PROTOCOL_EVENT_LOG_BYTES
        {
            return Err(invalid("AG-UI event log exceeds its storage limit"));
        }
        ensure_private_directory(&self.events_root)?;
        if fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(invalid("AG-UI event log is unavailable"));
        }
        let mut options = fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let write = (|| -> Result<(), AppError> {
            let mut file = options.open(&path)?;
            use std::io::Write;
            file.write_all(&encoded)?;
            file.write_all(b"\n")?;
            #[cfg(test)]
            if self.fail_next_event_sync.swap(false, Ordering::AcqRel) {
                return Err(std::io::Error::other("injected protocol event sync failure").into());
            }
            file.sync_data()?;
            // A newly created event filename must be durable too.
            fs::File::open(&self.events_root)?.sync_all()?;
            Ok(())
        })();
        if write.is_err() {
            self.quarantine_storage();
        }
        write?;
        Ok(sequence)
    }
    /// Read-only bounded replay for a public runId; the cursor is a transport
    /// sequence, not an event field and does not start/retry model execution.
    pub fn protocol_events(
        &self,
        owner: &Owner,
        run_id: &str,
        after_sequence: u64,
    ) -> Result<ProtocolEventPage, AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        let thread_id = record
            .protocol_thread_id
            .as_deref()
            .ok_or_else(|| invalid("Agent task is not an AG-UI run"))?;
        if record.protocol_commit_pending.is_some() || protocol_storage_failed(record) {
            return Err(invalid("AG-UI event log is unavailable"));
        }
        let task_id = record.task.run_id.clone();
        let path = self.event_path(&task_id)?;
        let events = read_protocol_events(&path, Some((thread_id, &record.task.request_id)))?;
        let latest = events.last().map_or(0, |entry| entry.sequence);
        if record.task.state.terminal()
            && !events
                .last()
                .is_some_and(|entry| is_terminal_event(&entry.event))
        {
            return Err(invalid("AG-UI event stream ended without a terminal event"));
        }
        if after_sequence > latest {
            return Err(invalid("invalid AG-UI event cursor"));
        }
        let page = events
            .iter()
            .filter(|entry| entry.sequence > after_sequence)
            .take(MAX_PROTOCOL_EVENT_PAGE)
            .cloned()
            .collect::<Vec<_>>();
        let sequence = page.last().map_or(after_sequence, |entry| entry.sequence);
        Ok(ProtocolEventPage {
            events: page.into_iter().map(|entry| entry.event).collect(),
            sequence,
        })
    }
    fn event_path(&self, task_id: &str) -> Result<PathBuf, AppError> {
        protocol_event_path(&self.events_root, task_id)
    }
    /// Return task states for startup reconciliation across App release
    /// upgrades. This host-only view deliberately ignores release digest and
    /// does not expose input, result, or tool data.
    pub fn recovery_states(&self, app_id: &str) -> Result<BTreeMap<String, State>, AppError> {
        let owner = Owner {
            app_id: app_id.to_owned(),
            release_digest: "0".repeat(64),
            invocation_id: "startup-recovery".into(),
        };
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        Ok(ledger
            .runs
            .values()
            .filter(|record| record.owner.app_id == app_id)
            .map(|record| (record.task.run_id.clone(), record.task.state.clone()))
            .collect())
    }
    /// App/release-scoped durable task listing. A refreshed page can recover
    /// its prior run while active execution leases remain invocation-scoped.
    pub fn list(
        &self,
        owner: &Owner,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Task>, AppError> {
        owner.validate()?;
        if !(1..=100).contains(&limit) {
            return Err(invalid("invalid task list limit"));
        }
        self.list_rows(owner, cursor, limit)
    }
    /// Return one public-cursor page while preserving the stable store order.
    /// `cursor` is a request ID, never a private task-ledger path or key.
    pub fn list_page(
        &self,
        owner: &Owner,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TaskListPage, AppError> {
        owner.validate()?;
        if !(1..=100).contains(&limit) {
            return Err(invalid("invalid task list limit"));
        }
        let private_cursor = cursor
            .map(|request_id| self.task_id_for_request(owner, request_id))
            .transpose()?;
        let mut tasks = self.list_rows(owner, private_cursor.as_deref(), limit + 1)?;
        let has_more = tasks.len() > limit;
        tasks.truncate(limit);
        let next_cursor = if has_more {
            tasks.last().map(|task| task.request_id.clone())
        } else {
            None
        };
        Ok(TaskListPage { tasks, next_cursor })
    }
    fn list_rows(
        &self,
        owner: &Owner,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Task>, AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let ledger = self.lock_ledger()?;
        let mut tasks = ledger
            .runs
            .values()
            .filter(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
            })
            .map(|record| {
                Self::reject_pending_commit(record)?;
                Ok(record.task.clone())
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        tasks.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        if let Some(cursor) = cursor {
            let index = tasks
                .iter()
                .position(|task| task.run_id == cursor)
                .ok_or_else(|| invalid("invalid task cursor"))?;
            tasks.drain(..=index);
        }
        tasks.truncate(limit);
        Ok(tasks)
    }
    /// Recheck before execution, after approval, reconnect and authentication
    /// retry. Native hosts additionally recheck the actual active app run.
    pub fn lease_valid(&self, owner: &Owner, id: &str) -> bool {
        self.lock_ledger().ok().is_some_and(|ledger| {
            ledger.runs.get(id).is_some_and(|r| {
                r.owner == *owner
                    && !r.cancel_requested
                    && r.protocol_commit_pending.is_none()
                    && !r.task.state.terminal()
                    && ledger.grants.get(&owner.app_id).is_some_and(|g| {
                        g.epoch == r.epoch && g.owner.release_digest == owner.release_digest
                    })
            })
        })
    }
    /// Cancel intent is durable; admission stays occupied until foreground reap.
    pub fn request_cancel(&self, owner: &Owner, id: &str) -> Result<(), AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|r| {
                    r.owner.app_id == owner.app_id && r.owner.release_digest == owner.release_digest
                })
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            Self::reject_pending_commit(record)?;
            if !record.task.state.terminal() {
                record.cancel_requested = true;
            }
            Ok(())
        })
    }

    /// Persist the trusted iOS system-expiration intent before revoking the
    /// executor. The caller must only use this for an actual native callback.
    /// A prior cancellation, authorization change or terminal result wins.
    pub fn request_background_expiration(&self, owner: &Owner, id: &str) -> Result<(), AppError> {
        let _commit = self
            .protocol_commit
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        owner.validate()?;
        let mut ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .get(id)
            .filter(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        let current_grant = ledger.grants.get(&owner.app_id).is_some_and(|grant| {
            grant.epoch == record.epoch && grant.owner.release_digest == owner.release_digest
        });
        if record.protocol_commit_pending.is_some() {
            return Err(Self::storage_unavailable());
        }
        if record.task.state.terminal() || record.cancel_requested || !current_grant {
            return Ok(());
        }
        let mut next = ledger.clone();
        let record = next.runs.get_mut(id).expect("checked Agent task");
        record.cancel_requested = true;
        record.stop_intent = Some(StopIntent::IosBackgroundExpired);
        if let Err(error) = self.persist(&next) {
            // Rename may already have succeeded. Do not let the old in-memory
            // ledger be read or written until a new store recovers the disk.
            self.storage_uncertain.store(true, Ordering::Release);
            return Err(error);
        }
        *ledger = next;
        Ok(())
    }
    /// Publish a trusted execution phase without revealing approval inputs.
    /// Its sequence is durable, so a reconnected browser can observe a pending
    /// native question or permission request without gaining an answer API.
    pub fn set_phase(&self, owner: &Owner, id: &str, phase: Option<&str>) -> Result<(), AppError> {
        if phase.is_some_and(|p| !matches!(p, "waiting_for_permission" | "waiting_for_answer")) {
            return Err(invalid("invalid Agent phase"));
        }
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|r| r.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            if record.protocol_commit_pending.is_some() {
                return Err(Self::storage_unavailable());
            }
            if !record.task.state.terminal() && record.task.phase.as_deref() != phase {
                record.task.phase = phase.map(String::from);
                record.task.sequence += 1;
                record.task.updated_at = now();
            }
            Ok(())
        })
    }
    /// Persist a trusted-host model-stream observation once the executor has stopped.
    /// It does not alter the public task, its sequence, or the AG-UI event log.
    pub fn record_native_output_bytes(
        &self,
        owner: &Owner,
        id: &str,
        revision: u64,
        bytes: u64,
    ) -> Result<(), AppError> {
        if bytes == 0 || bytes > MAX_NATIVE_OUTPUT_BYTES {
            return Err(invalid("invalid native output byte count"));
        }
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|record| record.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            if record.protocol_commit_pending.is_some() || record.task.state.terminal() {
                return Err(invalid("Agent task already ended"));
            }
            record.native_output = Some(NativeOutputObservation { revision, bytes });
            Ok(())
        })
    }

    /// Retrieve only the host-private observation for an owner-scoped public run ID.
    pub fn native_output_bytes_for_request(
        &self,
        owner: &Owner,
        run_id: &str,
        revision: u64,
    ) -> Result<Option<u64>, AppError> {
        owner.validate()?;
        let ledger = self.lock_ledger()?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        Self::reject_pending_commit(record)?;
        Ok(record
            .native_output
            .as_ref()
            .filter(|observation| observation.revision == revision)
            .map(|observation| observation.bytes.min(MAX_NATIVE_OUTPUT_BYTES)))
    }

    /// Publish a confirmed completed-model-turn count when the executor knows it.
    pub fn set_completed_model_turns(
        &self,
        owner: &Owner,
        id: &str,
        completed_model_turns: u32,
    ) -> Result<(), AppError> {
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|record| record.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            if record.protocol_commit_pending.is_some() {
                return Err(Self::storage_unavailable());
            }
            if record.task.state.terminal() {
                return Ok(());
            }
            if completed_model_turns == 0
                || completed_model_turns > record.task.limits.max_turns
                || record
                    .task
                    .progress
                    .as_ref()
                    .is_some_and(|progress| completed_model_turns <= progress.completed_model_turns)
            {
                return Ok(());
            }
            record.task.progress = Some(TaskProgress {
                completed_model_turns,
                turn_limit: record.task.limits.max_turns,
            });
            record.task.sequence += 1;
            record.task.updated_at = now();
            Ok(())
        })
    }
    /// Host executor only; terminal states are immutable. Cancellation is only
    /// finalized after the foreground/tool executor has stopped and reaped.
    pub fn update(
        &self,
        owner: &Owner,
        id: &str,
        state: State,
        result: Option<TaskResult>,
        usage: Option<Value>,
        error: Option<String>,
    ) -> Result<Task, AppError> {
        self.change(|ledger| {
            let authorized = ledger.runs.get(id).is_some_and(|record| {
                !record.cancel_requested
                    && ledger.grants.get(&owner.app_id).is_some_and(|grant| {
                        grant.epoch == record.epoch
                            && grant.owner.release_digest == owner.release_digest
                    })
            });
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|r| r.owner == *owner)
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            if record.protocol_commit_pending.is_some() {
                return Err(Self::storage_unavailable());
            }
            if record.task.state.terminal() {
                return Ok(record.task.clone());
            }
            if record.protocol_thread_id.is_some() && state.terminal() {
                return Err(invalid("AG-UI terminal state requires a protocol commit"));
            }
            record.task = terminal_candidate(record, authorized, state, result, usage, error)?;
            Ok(record.task.clone())
        })
    }

    /// If the standard terminal event could not be durably recorded, prevent
    /// private task metadata from claiming a successful public run. Preserve an
    /// existing result as evidence without treating it as a successful outcome.
    pub fn interrupt_after_protocol_failure(
        &self,
        owner: &Owner,
        id: &str,
        code: &str,
    ) -> Result<Task, AppError> {
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|record| record.owner == *owner && record.protocol_thread_id.is_some())
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            Self::reject_pending_commit(record)?;
            mark_protocol_failure(record, code);
            Ok(record.task.clone())
        })
    }
}

#[cfg(test)]
fn commit_protocol_terminal_for_test(
    store: &AgentStore,
    owner: &Owner,
    id: &str,
    state: State,
    result: Option<TaskResult>,
    error: Option<String>,
) -> Result<Task, AppError> {
    let (thread_id, request_id) = {
        let ledger = store.lock_ledger()?;
        let record = ledger
            .runs
            .get(id)
            .filter(|record| record.owner == *owner)
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        (
            record
                .protocol_thread_id
                .clone()
                .ok_or_else(|| invalid("Agent task is not an AG-UI run"))?,
            record.task.request_id.clone(),
        )
    };
    store
        .with_protocol_commit_permit(|permit| {
            let candidate = store.prepare_protocol_terminal(
                permit,
                owner, id, state, result, None, error,
            )?;
            if candidate.state == State::Succeeded {
                let result = candidate
                    .result
                    .as_ref()
                    .ok_or_else(|| invalid("successful test task has no result"))?;
                let message_id = result
                    .message_id
                    .as_deref()
                    .ok_or_else(|| invalid("successful test task has no message ID"))?;
                for event in [
                    json!({"type":"TEXT_MESSAGE_START","messageId":message_id,"role":"assistant"}),
                    json!({"type":"TEXT_MESSAGE_CONTENT","messageId":message_id,"delta":result.text.clone()}),
                    json!({"type":"TEXT_MESSAGE_END","messageId":message_id}),
                ] {
                    store.append_protocol_commit_event(permit, owner, id, event)?;
                }
            }
            let terminal = match candidate.state {
                State::Succeeded => {
                    let mut event = json!({
                        "type":"RUN_FINISHED","threadId":thread_id,"runId":request_id,
                        "outcome":{"type":"success"}
                    });
                    if let Some(output) = candidate
                        .result
                        .as_ref()
                        .and_then(|result| result.output.as_ref())
                        .filter(|output| !output.is_null())
                    {
                        event["result"] = output.clone();
                    }
                    event
                }
                State::Cancelled => json!({
                    "type":"RUN_FINISHED","threadId":thread_id,"runId":request_id,
                    "outcome":{"type":"cancelled"}
                }),
                State::Failed | State::Interrupted => json!({
                    "type":"RUN_ERROR",
                    "message":candidate.error.as_ref().map_or("The Agent run did not complete.", |error| error.message.as_str()),
                    "code":candidate.error.as_ref().map_or("agent_failed", |error| error.code.as_str())
                }),
                State::Queued | State::Running => {
                    return Err(invalid("invalid Agent terminal state"));
                }
            };
            store.append_protocol_commit_event(permit, owner, id, terminal)?;
            store.finalize_protocol_terminal(permit, owner, id)
        })?
}

fn ensure_private_directory(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(invalid("Agent storage directory is unavailable"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("Agent storage directory is unavailable"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn protocol_event_path(events_root: &Path, task_id: &str) -> Result<PathBuf, AppError> {
    let uuid = uuid::Uuid::parse_str(task_id)
        .ok()
        .filter(|uuid| uuid.to_string() == task_id)
        .ok_or_else(|| invalid("invalid internal Agent task ID"))?;
    let path = events_root.join(format!("{uuid}.jsonl"));
    if !path.starts_with(events_root) {
        return Err(invalid("invalid internal Agent task ID"));
    }
    Ok(path)
}

fn validate_event_identity(
    event: &Value,
    expected_thread_id: &str,
    expected_run_id: &str,
) -> Result<(), AppError> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(event_type, "RUN_STARTED" | "RUN_FINISHED")
        && (event.get("threadId").and_then(Value::as_str) != Some(expected_thread_id)
            || event.get("runId").and_then(Value::as_str) != Some(expected_run_id))
    {
        return Err(invalid("AG-UI event identity does not match its run"));
    }
    Ok(())
}

fn is_terminal_event(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("RUN_FINISHED" | "RUN_ERROR")
    )
}

fn read_protocol_events(
    path: &Path,
    identity: Option<(&str, &str)>,
) -> Result<Vec<SequencedEvent>, AppError> {
    read_protocol_events_with_validator(path, identity, &protocol::validate_event)
        .map_err(Into::into)
}

fn read_protocol_events_with_validator(
    path: &Path,
    identity: Option<(&str, &str)>,
    validate_event: &impl Fn(&Value) -> Result<(), protocol::ProtocolError>,
) -> Result<Vec<SequencedEvent>, EventLogReadError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(EventLogReadError::Unavailable(error.into())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EventLogReadError::Unavailable(AppError::Io(
            std::io::Error::new(std::io::ErrorKind::Other, "AG-UI event log is unavailable"),
        )));
    }
    if metadata.len() > MAX_PROTOCOL_EVENT_LOG_BYTES {
        return Err(EventLogReadError::Corrupt(
            "AG-UI event log exceeds its storage limit",
        ));
    }
    let bytes = fs::read(path).map_err(|error| EventLogReadError::Unavailable(error.into()))?;
    let encoded = std::str::from_utf8(&bytes)
        .map_err(|_| EventLogReadError::Corrupt("AG-UI event log is invalid"))?;
    if !encoded.is_empty() && !encoded.ends_with('\n') {
        return Err(EventLogReadError::Corrupt("AG-UI event log is incomplete"));
    }
    let mut events = Vec::new();
    for line in encoded.lines() {
        let entry: SequencedEvent = serde_json::from_str(line)
            .map_err(|_| EventLogReadError::Corrupt("AG-UI event log is invalid"))?;
        validate_event(&entry.event).map_err(|error| {
            if error.code == "protocol_schema_unavailable" {
                EventLogReadError::Unavailable(protocol_schema_unavailable())
            } else {
                EventLogReadError::Corrupt("AG-UI event does not match the pinned protocol")
            }
        })?;
        if let Some((thread_id, run_id)) = identity {
            validate_event_identity(&entry.event, thread_id, run_id).map_err(|_| {
                EventLogReadError::Corrupt("AG-UI event identity does not match its run")
            })?;
        }
        if entry.sequence != events.len() as u64 + 1
            || (events.is_empty() && entry.event["type"] != "RUN_STARTED")
            || (!events.is_empty() && entry.event["type"] == "RUN_STARTED")
            || events
                .last()
                .is_some_and(|last: &SequencedEvent| is_terminal_event(&last.event))
        {
            return Err(EventLogReadError::Corrupt(
                "AG-UI event log ordering is invalid",
            ));
        }
        events.push(entry);
    }
    Ok(events)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod configure_tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn owner(app_digit: char) -> Owner {
        Owner {
            app_id: format!("a{}", app_digit.to_string().repeat(32)),
            release_digest: "a".repeat(64),
            invocation_id: "original-invocation".into(),
        }
    }

    fn settings() -> Settings {
        Settings {
            model: Model {
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            tools: BTreeMap::from([(
                "tool-id".into(),
                ToolGrant::Builtin {
                    name: "tool".into(),
                },
            )]),
            skills: vec!["review".into()],
            background: false,
            max_turns: 10,
            timeout_ms: 60_000,
        }
    }

    fn input() -> Input {
        Input {
            request_id: "request".into(),
            protocol_thread_id: None,
            protocol_parent_run_id: None,
            protocol_input_digest: None,
            protocol_message_sha256: None,
            protocol_history_digest: None,
            prompt: "Review this".into(),
            attachments: vec![],
            content_parts: vec![],
            output_schema: None,
            tools: vec![],
            skills: vec![],
            max_turns: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn unchanged_grant_keeps_the_existing_run_and_ledger_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let store = AgentStore::open(directory.path()).unwrap();
        let run_owner = owner('1');
        let grant = settings();
        assert_eq!(
            store
                .configure_with_change(run_owner.clone(), Some(grant.clone()))
                .unwrap(),
            ConfigureResult {
                changed: true,
                affected_run_ids: vec![],
            }
        );
        let run = store.admit(&run_owner, &input(), &[]).unwrap().task;
        let (epoch, grant_epoch) = {
            let ledger = store.ledger.lock().unwrap();
            (ledger.epoch, ledger.grants[&run_owner.app_id].epoch)
        };
        let persisted = fs::read(&store.path).unwrap();

        // The invocation identifies the caller, not a different release grant.
        let settings_owner = Owner {
            invocation_id: "native-settings".into(),
            ..run_owner.clone()
        };
        assert_eq!(
            store
                .configure_with_change(settings_owner, Some(grant))
                .unwrap(),
            ConfigureResult {
                changed: false,
                affected_run_ids: vec![],
            }
        );
        assert!(store.lease_valid(&run_owner, &run.run_id));
        let ledger = store.ledger.lock().unwrap();
        assert_eq!(ledger.epoch, epoch);
        assert_eq!(ledger.grants[&run_owner.app_id].epoch, grant_epoch);
        assert_eq!(fs::read(&store.path).unwrap(), persisted);
    }

    #[test]
    fn changed_settings_and_release_revoke_only_the_matching_app() {
        let directory = tempfile::tempdir().unwrap();
        let store = AgentStore::open(directory.path()).unwrap();
        let first = owner('1');
        let second = owner('2');
        let grant = settings();
        store.configure(first.clone(), Some(grant.clone())).unwrap();
        store
            .configure(second.clone(), Some(grant.clone()))
            .unwrap();
        let first_run = store.admit(&first, &input(), &[]).unwrap().task;
        let second_run = store.admit(&second, &input(), &[]).unwrap().task;
        let initial_epoch = store.ledger.lock().unwrap().epoch;

        let mut changed = grant.clone();
        changed.timeout_ms -= 1;
        assert_eq!(
            store
                .configure_with_change(first.clone(), Some(changed))
                .unwrap(),
            ConfigureResult {
                changed: true,
                affected_run_ids: vec![first_run.run_id.clone()],
            }
        );
        assert!(!store.lease_valid(&first, &first_run.run_id));
        assert!(store.lease_valid(&second, &second_run.run_id));
        assert_eq!(store.ledger.lock().unwrap().epoch, initial_epoch + 1);

        let new_release = Owner {
            release_digest: "b".repeat(64),
            ..second.clone()
        };
        assert_eq!(
            store
                .configure_with_change(new_release.clone(), Some(grant))
                .unwrap(),
            ConfigureResult {
                changed: true,
                affected_run_ids: vec![second_run.run_id.clone()],
            }
        );
        assert!(!store.lease_valid(&second, &second_run.run_id));
        assert!(store.settings(&second).unwrap().is_none());
        assert!(store.settings(&new_release).unwrap().is_some());
        assert_eq!(store.ledger.lock().unwrap().epoch, initial_epoch + 2);
    }

    #[test]
    fn explicit_revoke_and_regrant_do_not_revive_a_run() {
        let directory = tempfile::tempdir().unwrap();
        let store = AgentStore::open(directory.path()).unwrap();
        let run_owner = owner('1');
        let grant = settings();
        store
            .configure(run_owner.clone(), Some(grant.clone()))
            .unwrap();
        let run = store.admit(&run_owner, &input(), &[]).unwrap().task;
        assert_eq!(
            store
                .configure_with_change(run_owner.clone(), None)
                .unwrap(),
            ConfigureResult {
                changed: true,
                affected_run_ids: vec![run.run_id.clone()],
            }
        );
        assert!(!store.lease_valid(&run_owner, &run.run_id));
        assert!(
            store
                .configure_with_change(run_owner.clone(), None)
                .unwrap()
                .changed
        );
        store.configure(run_owner.clone(), Some(grant)).unwrap();
        assert!(!store.lease_valid(&run_owner, &run.run_id));
    }

    #[test]
    fn unchanged_configure_and_revoke_are_serialized_against_the_same_grant() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AgentStore::open(directory.path()).unwrap());
        let run_owner = owner('1');
        let grant = settings();
        store
            .configure(run_owner.clone(), Some(grant.clone()))
            .unwrap();
        let run = store.admit(&run_owner, &input(), &[]).unwrap().task;
        let initial_epoch = store.ledger.lock().unwrap().epoch;
        let barrier = Arc::new(Barrier::new(3));

        let keep_store = store.clone();
        let keep_owner = run_owner.clone();
        let keep_grant = grant.clone();
        let keep_barrier = barrier.clone();
        let keep = std::thread::spawn(move || {
            keep_barrier.wait();
            keep_store
                .configure_with_change(keep_owner, Some(keep_grant))
                .unwrap()
        });
        let revoke_store = store.clone();
        let revoke_owner = run_owner.clone();
        let revoke_barrier = barrier.clone();
        let revoke = std::thread::spawn(move || {
            revoke_barrier.wait();
            revoke_store
                .configure_with_change(revoke_owner, None)
                .unwrap()
        });
        barrier.wait();
        let kept = keep.join().unwrap();
        let revoked = revoke.join().unwrap();

        assert!(revoked.changed);
        assert_eq!(revoked.affected_run_ids, vec![run.run_id.clone()]);
        assert!(!store.lease_valid(&run_owner, &run.run_id));
        let ledger = store.ledger.lock().unwrap();
        if ledger.grants.contains_key(&run_owner.app_id) {
            assert!(kept.changed);
            assert_eq!(kept.affected_run_ids, vec![run.run_id]);
            assert_eq!(ledger.epoch, initial_epoch + 2);
        } else {
            assert!(!kept.changed);
            assert!(kept.affected_run_ids.is_empty());
            assert_eq!(ledger.epoch, initial_epoch + 1);
        }
    }
}

#[cfg(test)]
mod protocol_failure_tests {
    use super::*;

    fn owner() -> Owner {
        Owner {
            app_id: format!("a{}", "1".repeat(32)),
            release_digest: "a".repeat(64),
            invocation_id: "page".into(),
        }
    }

    pub(super) fn input(request_id: &str) -> Input {
        serde_json::from_value(json!({
            "requestId": request_id,
            "protocolThreadId": "thread-1",
            "prompt": "Do the requested work"
        }))
        .unwrap()
    }

    pub(super) fn setup() -> (tempfile::TempDir, AgentStore, Owner) {
        let root = tempfile::tempdir().unwrap();
        let store = AgentStore::open(root.path()).unwrap();
        let owner = owner();
        let settings = serde_json::from_value(json!({
            "model": {"providerId": "provider", "modelId": "model"},
            "tools": {}, "skills": [], "background": false,
            "maxTurns": 4, "timeoutMs": 30_000
        }))
        .unwrap();
        store.configure(owner.clone(), Some(settings)).unwrap();
        (root, store, owner)
    }

    fn started_event(request_id: &str) -> Vec<u8> {
        serde_json::to_vec(&SequencedEvent {
            sequence: 1,
            event: json!({"type":"RUN_STARTED", "protocolVersion":"1.0",
                         "threadId":"thread-1", "runId":request_id}),
        })
        .unwrap()
    }

    fn encode_events(events: Vec<Value>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, event) in events.into_iter().enumerate() {
            bytes.extend(
                serde_json::to_vec(&SequencedEvent {
                    sequence: index as u64 + 1,
                    event,
                })
                .unwrap(),
            );
            bytes.push(b'\n');
        }
        bytes
    }

    fn force_legacy_terminal_for_recovery_test(
        store: &AgentStore,
        id: &str,
        state: State,
        result: Option<TaskResult>,
        error: Option<String>,
    ) {
        let mut ledger = store.lock_ledger().unwrap();
        let record = ledger.runs.get_mut(id).unwrap();
        record.task.state = state;
        record.task.phase = None;
        record.task.sequence = record.task.sequence.saturating_add(1);
        record.task.updated_at = now();
        record.task.result = result;
        record.task.error = error.map(|code| TaskError {
            message: error_message(&code).into(),
            code,
        });
        store.persist(&ledger).unwrap();
    }

    #[test]
    fn failed_unstarted_event_preserves_bytes_and_does_not_replay() {
        let mut complete = started_event("failed-run");
        complete.push(b'\n');
        let cases = [
            Some(b"{\"sequence\":1,\"event\":".to_vec()),
            Some(started_event("failed-run")),
            Some(Vec::new()),
            // A complete row may still have failed during sync_data.
            Some(complete),
            None,
        ];
        for bytes in cases {
            let (root, store, owner) = setup();
            let original = input("failed-run");
            let run = store.admit(&owner, &original, &[]).unwrap().task;
            let path = store.event_path(&run.run_id).unwrap();
            if let Some(bytes) = &bytes {
                fs::write(&path, bytes).unwrap();
            }

            let interrupted = store
                .interrupt_unstarted_protocol_admission(&owner, &run.run_id)
                .unwrap();
            assert_eq!(interrupted.state, State::Interrupted);
            assert!(interrupted.result.is_none());
            assert_eq!(
                interrupted.error.unwrap().code,
                PROTOCOL_EVENT_STORAGE_FAILED
            );
            assert!(store.protocol_events(&owner, "failed-run", 0).is_err());
            assert!(store
                .append_event(
                    &owner,
                    &run.run_id,
                    json!({
                        "type":"RUN_STARTED", "threadId":"thread-1", "runId":"failed-run"
                    })
                )
                .is_err());
            assert!(!store.admit(&owner, &original, &[]).unwrap().start);
            drop(store);

            for _ in 0..2 {
                let store = AgentStore::open(root.path()).unwrap();
                let recovered = store.get_by_request(&owner, "failed-run").unwrap();
                assert_eq!(recovered.state, State::Interrupted);
                assert_eq!(recovered.error.unwrap().code, PROTOCOL_EVENT_STORAGE_FAILED);
                assert!(store.protocol_events(&owner, "failed-run", 0).is_err());
                match &bytes {
                    Some(expected) => assert_eq!(&fs::read(&path).unwrap(), expected),
                    None => assert!(!path.exists()),
                }
                drop(store);
            }
            let store = AgentStore::open(root.path()).unwrap();
            assert!(store.admit(&owner, &input("new-run"), &[]).unwrap().start);
        }
    }

    #[test]
    fn reopen_isolates_a_corrupt_or_empty_run_without_changing_its_bytes() {
        let cases = [
            b"{\"sequence\":1,\"event\":".to_vec(),
            started_event("damaged-run"),
            Vec::new(),
        ];
        for bytes in cases {
            let (root, store, owner) = setup();
            let run = store
                .admit(&owner, &input("damaged-run"), &[])
                .unwrap()
                .task;
            let path = store.event_path(&run.run_id).unwrap();
            fs::write(&path, &bytes).unwrap();
            drop(store);

            for _ in 0..2 {
                let store = AgentStore::open(root.path()).unwrap();
                let task = store.get_by_request(&owner, "damaged-run").unwrap();
                assert_eq!(task.state, State::Interrupted);
                assert_eq!(task.error.unwrap().code, PROTOCOL_EVENT_STORAGE_FAILED);
                assert!(store.protocol_events(&owner, "damaged-run", 0).is_err());
                assert_eq!(fs::read(&path).unwrap(), bytes);
                drop(store);
            }
            let store = AgentStore::open(root.path()).unwrap();
            assert!(
                store
                    .admit(&owner, &input("another-run"), &[])
                    .unwrap()
                    .start
            );
        }
    }

    #[test]
    fn mismatched_identities_and_duplicate_started_are_isolated() {
        let started = json!({"type":"RUN_STARTED", "threadId":"thread-1", "runId":"damaged-run"});
        let finished = json!({"type":"RUN_FINISHED", "threadId":"thread-1", "runId":"damaged-run"});
        let cases = [
            vec![json!({"type":"RUN_STARTED", "threadId":"other", "runId":"damaged-run"})],
            vec![
                started.clone(),
                json!({"type":"RUN_FINISHED", "threadId":"thread-1", "runId":"other"}),
            ],
            vec![started.clone(), started.clone(), finished.clone()],
        ];
        for events in cases {
            let (root, store, owner) = setup();
            let run = store
                .admit(&owner, &input("damaged-run"), &[])
                .unwrap()
                .task;
            let path = store.event_path(&run.run_id).unwrap();
            let bytes = encode_events(events);
            fs::write(&path, &bytes).unwrap();
            assert!(store.protocol_events(&owner, "damaged-run", 0).is_err());
            assert!(store
                .append_event(&owner, &run.run_id, finished.clone())
                .is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            drop(store);

            let reopened = AgentStore::open(root.path()).unwrap();
            let task = reopened.get_by_request(&owner, "damaged-run").unwrap();
            assert_eq!(task.state, State::Interrupted);
            assert_eq!(task.error.unwrap().code, PROTOCOL_EVENT_STORAGE_FAILED);
            assert!(reopened.protocol_events(&owner, "damaged-run", 0).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn unavailable_event_reader_does_not_mutate_the_ledger() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("unavailable-run"), &[])
            .unwrap()
            .task;
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "threadId":"thread-1", "runId":"unavailable-run"
                }),
            )
            .unwrap();
        let event_path = store.event_path(&run.run_id).unwrap();
        let ledger_path = store.path.clone();
        let ledger_before = fs::read(&ledger_path).unwrap();
        let event_before = fs::read(&event_path).unwrap();
        drop(store);

        let error = AgentStore::open_with_event_validator(root.path(), |_| {
            Err(protocol::ProtocolError {
                code: "protocol_schema_unavailable",
                message: "schema unavailable",
            })
        });
        assert!(matches!(error, Err(AppError::Io(_))));
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        assert_eq!(fs::read(&event_path).unwrap(), event_before);

        let error = AgentStore::open_with_event_validator(root.path(), |event| {
            if event["runId"] == "unavailable-run" {
                Err(protocol::ProtocolError {
                    code: "protocol_schema_unavailable",
                    message: "schema unavailable",
                })
            } else {
                protocol::validate_event(event)
            }
        });
        assert!(matches!(error, Err(AppError::Io(_))));
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        assert_eq!(fs::read(&event_path).unwrap(), event_before);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&event_path, fs::Permissions::from_mode(0o000)).unwrap();
            // A privileged test runner may still be able to read mode 000.
            if fs::read(&event_path).is_err() {
                assert!(matches!(
                    AgentStore::open(root.path()),
                    Err(AppError::Io(_))
                ));
                assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
            }
            fs::set_permissions(&event_path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let backup = event_path.with_extension("test-backup");
        fs::rename(&event_path, &backup).unwrap();
        fs::create_dir(&event_path).unwrap();
        assert!(matches!(
            AgentStore::open(root.path()),
            Err(AppError::Io(_))
        ));
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        fs::remove_dir(&event_path).unwrap();
        fs::rename(&backup, &event_path).unwrap();

        // The file becomes unavailable after the first successful read, while
        // recovery is about to append a terminal event.
        let changed = std::cell::Cell::new(false);
        let error = AgentStore::open_with_event_validator(root.path(), |event| {
            if event["runId"] == "unavailable-run" && !changed.replace(true) {
                fs::rename(&event_path, &backup).unwrap();
                fs::create_dir(&event_path).unwrap();
            }
            protocol::validate_event(event)
        });
        assert!(matches!(error, Err(AppError::Io(_))));
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        fs::remove_dir(&event_path).unwrap();
        fs::rename(&backup, &event_path).unwrap();
        assert_eq!(fs::read(&event_path).unwrap(), event_before);

        let reopened = AgentStore::open(root.path()).unwrap();
        assert_eq!(
            reopened
                .get_by_request(&owner, "unavailable-run")
                .unwrap()
                .state,
            State::Interrupted
        );
    }

    #[test]
    fn partial_multi_run_recovery_keeps_the_ledger_and_retries_only_event_recovery() {
        let (root, store, owner) = setup();
        let mut runs = Vec::new();
        for request_id in ["recovery-a", "recovery-b"] {
            let run = store.admit(&owner, &input(request_id), &[]).unwrap().task;
            store
                .append_event(
                    &owner,
                    &run.run_id,
                    json!({
                        "type":"RUN_STARTED", "threadId":"thread-1", "runId":request_id
                    }),
                )
                .unwrap();
            force_legacy_terminal_for_recovery_test(
                &store,
                &run.run_id,
                State::Failed,
                None,
                Some("agent_failed".into()),
            );
            runs.push((
                run.run_id.clone(),
                request_id,
                store.event_path(&run.run_id).unwrap(),
            ));
        }
        runs.sort_by(|a, b| a.0.cmp(&b.0));
        let first_path = runs[0].2.clone();
        let second_path = runs[1].2.clone();
        let second_request = runs[1].1;
        let backup = second_path.with_extension("test-backup");
        let first_before = fs::read(&first_path).unwrap();
        let second_before = fs::read(&second_path).unwrap();
        let ledger_path = store.path.clone();
        let ledger_before = fs::read(&ledger_path).unwrap();
        drop(store);

        let changed = std::cell::Cell::new(false);
        let error = AgentStore::open_with_event_validator(root.path(), |event| {
            if event["runId"] == second_request && !changed.replace(true) {
                fs::rename(&second_path, &backup).unwrap();
                fs::create_dir(&second_path).unwrap();
            }
            protocol::validate_event(event)
        });
        assert!(matches!(error, Err(AppError::Io(_))));
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        let first_after = fs::read(&first_path).unwrap();
        assert!(first_after.starts_with(&first_before));
        assert!(first_after.len() > first_before.len());
        assert_eq!(fs::read(&backup).unwrap(), second_before);
        fs::remove_dir(&second_path).unwrap();
        fs::rename(&backup, &second_path).unwrap();

        let reopened = AgentStore::open(root.path()).unwrap();
        for (_, request_id, _) in &runs {
            assert_eq!(
                reopened.get_by_request(&owner, request_id).unwrap().state,
                State::Failed
            );
            let page = reopened.protocol_events(&owner, request_id, 0).unwrap();
            assert_eq!(page.events.last().unwrap()["type"], "RUN_ERROR");
            assert_eq!(page.events.last().unwrap()["code"], "agent_failed");
            assert!(
                !reopened
                    .admit(&owner, &input(request_id), &[])
                    .unwrap()
                    .start
            );
        }
    }

    #[test]
    fn reopening_a_corrupt_completed_run_cannot_report_success() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("completed-run"), &[])
            .unwrap()
            .task;
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "threadId":"thread-1", "runId":"completed-run"
                }),
            )
            .unwrap();
        let result = TaskResult {
            text: "Previously persisted result evidence".into(),
            output: Some(json!({"answer": 42})),
            attachments: vec![],
            chat_id: None,
            message_id: Some("assistant-evidence".into()),
        };
        force_legacy_terminal_for_recovery_test(
            &store,
            &run.run_id,
            State::Succeeded,
            Some(result.clone()),
            None,
        );
        let path = store.event_path(&run.run_id).unwrap();
        let corrupt = b"{\"sequence\":1,\"event\":";
        fs::write(&path, corrupt).unwrap();
        drop(store);

        for _ in 0..2 {
            let reopened = AgentStore::open(root.path()).unwrap();
            let task = reopened.get_by_request(&owner, "completed-run").unwrap();
            assert_eq!(task.state, State::Interrupted);
            assert!(task.result.is_none());
            assert_eq!(task.error.unwrap().code, PROTOCOL_EVENT_STORAGE_FAILED);
            let ledger = reopened.lock_ledger().unwrap();
            assert_eq!(
                ledger.runs[&run.run_id]
                    .protocol_candidate_evidence
                    .as_ref()
                    .and_then(|task| task.result.as_ref()),
                Some(&result)
            );
            drop(ledger);
            assert!(reopened
                .protocol_events(&owner, "completed-run", 0)
                .is_err());
            assert_eq!(fs::read(&path).unwrap(), corrupt);
        }
        let reopened = AgentStore::open(root.path()).unwrap();
        assert!(
            reopened
                .admit(&owner, &input("other-run"), &[])
                .unwrap()
                .start
        );
    }

    #[test]
    fn terminal_event_and_ledger_conflicts_never_report_success() {
        let cases = [
            (
                State::Succeeded,
                Some(json!({"answer":42})),
                json!({
                    "type":"RUN_ERROR", "message":"The task was interrupted.", "code":"protocol_terminal_missing"
                }),
            ),
            (
                State::Succeeded,
                Some(Value::Null),
                json!({
                    "type":"RUN_ERROR", "message":"Null cannot be represented.", "code":"structured_output_not_representable"
                }),
            ),
            (
                State::Succeeded,
                Some(Value::Null),
                json!({
                    "type":"RUN_FINISHED", "threadId":"thread-1", "runId":"conflict-run", "outcome":{"type":"success"}
                }),
            ),
            (
                State::Succeeded,
                Some(json!({"answer":42})),
                json!({
                    "type":"RUN_FINISHED", "threadId":"thread-1", "runId":"conflict-run", "outcome":{"type":"success"}, "result":{"answer":99}
                }),
            ),
            (
                State::Failed,
                None,
                json!({
                    "type":"RUN_FINISHED", "threadId":"thread-1", "runId":"conflict-run", "outcome":{"type":"success"}
                }),
            ),
            (
                State::Queued,
                None,
                json!({
                    "type":"RUN_FINISHED", "threadId":"thread-1", "runId":"conflict-run", "outcome":{"type":"success"}
                }),
            ),
        ];
        for (state, output, terminal) in cases {
            let (root, store, owner) = setup();
            let run = store
                .admit(&owner, &input("conflict-run"), &[])
                .unwrap()
                .task;
            store
                .append_event(
                    &owner,
                    &run.run_id,
                    json!({
                        "type":"RUN_STARTED", "threadId":"thread-1", "runId":"conflict-run"
                    }),
                )
                .unwrap();
            let evidence = (state == State::Succeeded).then(|| TaskResult {
                text: "Saved evidence".into(),
                output,
                attachments: vec![],
                chat_id: None,
                message_id: None,
            });
            if state != State::Queued {
                force_legacy_terminal_for_recovery_test(
                    &store,
                    &run.run_id,
                    state,
                    evidence.clone(),
                    None,
                );
            }
            // The durable ledger still has the previous state when this
            // terminal has already been written (e.g. crash before ledger sync).
            store.append_event(&owner, &run.run_id, terminal).unwrap();
            let path = store.event_path(&run.run_id).unwrap();
            let bytes = fs::read(&path).unwrap();
            drop(store);
            for _ in 0..2 {
                let reopened = AgentStore::open(root.path()).unwrap();
                let task = reopened.get_by_request(&owner, "conflict-run").unwrap();
                assert_eq!(task.state, State::Interrupted);
                assert_eq!(task.error.unwrap().code, PROTOCOL_TERMINAL_CONFLICT);
                assert!(task.result.is_none());
                let ledger = reopened.lock_ledger().unwrap();
                assert_eq!(
                    ledger.runs[&run.run_id]
                        .protocol_candidate_evidence
                        .as_ref()
                        .and_then(|task| task.result.as_ref()),
                    evidence.as_ref()
                );
                drop(ledger);
                assert!(reopened.protocol_events(&owner, "conflict-run", 0).is_err());
                assert_eq!(fs::read(&path).unwrap(), bytes);
            }
            let reopened = AgentStore::open(root.path()).unwrap();
            assert!(
                reopened
                    .admit(&owner, &input("next-run"), &[])
                    .unwrap()
                    .start
            );
        }
    }

    #[test]
    fn matching_successful_terminal_remains_readable_after_restart() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("success-run"), &[])
            .unwrap()
            .task;
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "threadId":"thread-1", "runId":"success-run"
                }),
            )
            .unwrap();
        let result = TaskResult {
            text: "Saved answer".into(),
            output: Some(json!({"answer":42})),
            attachments: vec![],
            chat_id: None,
            message_id: Some("assistant-success".into()),
        };
        commit_protocol_terminal_for_test(
            &store,
            &owner,
            &run.run_id,
            State::Succeeded,
            Some(result.clone()),
            None,
        )
        .unwrap();
        drop(store);

        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened.get_by_request(&owner, "success-run").unwrap();
        assert_eq!(task.state, State::Succeeded);
        assert_eq!(task.result, Some(result));
        let events = reopened.protocol_events(&owner, "success-run", 0).unwrap();
        assert_eq!(events.events.last().unwrap()["type"], "RUN_FINISHED");
    }

    #[test]
    fn changed_success_text_event_is_quarantined_without_rewriting_event_bytes() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("changed-success-text"), &[])
            .unwrap()
            .task;
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"thread-1", "runId":"changed-success-text"
                }),
            )
            .unwrap();
        let result = TaskResult {
            text: "saved answer".into(),
            output: None,
            attachments: Vec::new(),
            chat_id: None,
            message_id: Some("assistant-changed-success".into()),
        };
        commit_protocol_terminal_for_test(
            &store,
            &owner,
            &run.run_id,
            State::Succeeded,
            Some(result.clone()),
            None,
        )
        .unwrap();
        let event_path = store.event_path(&run.run_id).unwrap();
        let original_bytes = fs::read(&event_path).unwrap();
        let mut changed_bytes = Vec::new();
        let mut changed_content = false;
        for line in std::str::from_utf8(&original_bytes).unwrap().lines() {
            let mut entry: SequencedEvent = serde_json::from_str(line).unwrap();
            if entry.event["type"] == "TEXT_MESSAGE_CONTENT" {
                entry.event["delta"] = json!("forged answer");
                changed_content = true;
            }
            changed_bytes.extend(serde_json::to_vec(&entry).unwrap());
            changed_bytes.push(b'\n');
        }
        assert!(changed_content);
        fs::write(&event_path, &changed_bytes).unwrap();
        drop(store);

        let recovered = AgentStore::open(root.path()).unwrap();
        let task = recovered
            .get_by_request(&owner, "changed-success-text")
            .unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert_eq!(
            task.error.as_ref().unwrap().code,
            PROTOCOL_TERMINAL_CONFLICT
        );
        assert_eq!(fs::read(&event_path).unwrap(), changed_bytes);
        assert!(recovered
            .protocol_events(&owner, "changed-success-text", 0)
            .is_err());
        let ledger = recovered.lock_ledger().unwrap();
        assert_eq!(
            ledger.runs[&run.run_id]
                .protocol_candidate_evidence
                .as_ref()
                .and_then(|task| task.result.as_ref()),
            Some(&result)
        );
    }

    #[test]
    fn full_started_bytes_and_failed_interruption_recover_as_host_restart() {
        let (root, mut store, owner) = setup();
        let original = input("uncertain-run");
        let run = store.admit(&owner, &original, &[]).unwrap().task;
        let event_path = store.event_path(&run.run_id).unwrap();
        let mut started = started_event("uncertain-run");
        started.push(b'\n');
        fs::write(&event_path, &started).unwrap();

        // Simulate a failed event sync and an independent ledger write failure.
        let ledger_path = store.path.clone();
        let blocked_path = ledger_path.with_extension("unwritable-directory");
        fs::create_dir(&blocked_path).unwrap();
        store.path = blocked_path;
        assert!(store
            .interrupt_unstarted_protocol_admission(&owner, &run.run_id)
            .is_err());
        assert!(matches!(
            store.get(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.admit(&owner, &input("new-run"), &[]),
            Err(AppError::Io(_))
        ));
        store.path = ledger_path;
        drop(store);

        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened.get_by_request(&owner, "uncertain-run").unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.error.unwrap().code, "host_restarted");
        assert!(!reopened.admit(&owner, &original, &[]).unwrap().start);
        let page = reopened
            .protocol_events(&owner, "uncertain-run", 0)
            .unwrap();
        assert_eq!(page.events.last().unwrap()["type"], "RUN_ERROR");
        assert_eq!(page.events.last().unwrap()["code"], "host_restarted");
        assert!(fs::read(&event_path).unwrap().starts_with(&started));
    }

    #[test]
    fn interrupt_after_terminal_write_failure_retains_result_evidence() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("terminal-failure"), &[])
            .unwrap()
            .task;
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "threadId":"thread-1", "runId":"terminal-failure"
                }),
            )
            .unwrap();
        let result = TaskResult {
            text: "Result evidence".into(),
            output: Some(json!({"value":1})),
            attachments: vec![],
            chat_id: None,
            message_id: Some("assistant-terminal-evidence".into()),
        };
        force_legacy_terminal_for_recovery_test(
            &store,
            &run.run_id,
            State::Succeeded,
            Some(result.clone()),
            None,
        );
        store
            .with_protocol_commit(|| {
                store.interrupt_after_protocol_failure(
                    &owner,
                    &run.run_id,
                    PROTOCOL_EVENT_STORAGE_FAILED,
                )
            })
            .unwrap()
            .unwrap();
        let path = store.event_path(&run.run_id).unwrap();
        let bytes = fs::read(&path).unwrap();
        drop(store);

        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened.get_by_request(&owner, "terminal-failure").unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert!(reopened
            .protocol_events(&owner, "terminal-failure", 0)
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let ledger = reopened.lock_ledger().unwrap();
        assert_eq!(
            ledger.runs[&run.run_id]
                .protocol_candidate_evidence
                .as_ref()
                .and_then(|task| task.result.as_ref()),
            Some(&result)
        );
    }

    #[test]
    fn failed_interrupt_quarantines_until_reopen_without_replay() {
        let (root, mut store, owner) = setup();
        let run = store
            .admit(&owner, &input("blocked-run"), &[])
            .unwrap()
            .task;
        let ledger_path = store.path.clone();
        let blocked_path = store.path.with_extension("unwritable-directory");
        fs::create_dir(&blocked_path).unwrap();
        store.path = blocked_path;

        assert!(store
            .interrupt_unstarted_protocol_admission(&owner, &run.run_id)
            .is_err());
        assert!(matches!(
            store.get(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.admit(&owner, &input("new-run"), &[]),
            Err(AppError::Io(_))
        ));
        store.path = ledger_path;
        drop(store);

        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened.get_by_request(&owner, "blocked-run").unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert_eq!(
            task.error.as_ref().unwrap().code,
            PROTOCOL_EVENT_STORAGE_FAILED
        );
        assert!(
            !reopened
                .admit(&owner, &input("blocked-run"), &[])
                .unwrap()
                .start
        );
        assert!(
            reopened
                .admit(&owner, &input("new-run"), &[])
                .unwrap()
                .start
        );
    }

    #[test]
    fn rollback_keeps_empty_event_file_even_if_it_cannot_be_unlinked() {
        let (root, store, owner) = setup();
        let original = input("rollback-run");
        let run = store.admit(&owner, &original, &[]).unwrap().task;
        let path = store.event_path(&run.run_id).unwrap();
        fs::write(&path, []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&store.events_root, fs::Permissions::from_mode(0o500)).unwrap();
        }
        let result = store.rollback_queued_admission(&owner, &run.run_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&store.events_root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        result.unwrap();
        assert!(path.is_file());
        assert!(fs::read(&path).unwrap().is_empty());
        assert!(store.get_by_request(&owner, "rollback-run").is_err());
        drop(store);
        let reopened = AgentStore::open(root.path()).unwrap();
        assert!(reopened.admit(&owner, &original, &[]).unwrap().start);
        assert!(path.is_file());
    }

    #[test]
    fn unstarted_failure_api_rejects_an_active_run() {
        let (_root, store, owner) = setup();
        let run = store.admit(&owner, &input("active-run"), &[]).unwrap().task;
        store
            .update(&owner, &run.run_id, State::Running, None, None, None)
            .unwrap();
        assert!(store
            .interrupt_unstarted_protocol_admission(&owner, &run.run_id)
            .is_err());
        assert_eq!(
            store.get(&owner, &run.run_id).unwrap().state,
            State::Running
        );
        assert!(matches!(
            store.admit(&owner, &input("new-run"), &[]),
            Err(AppError::Busy(_))
        ));
    }

    #[test]
    fn pending_terminal_candidate_is_hidden_after_recovery_and_duplicate_does_not_start() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("pending-run"), &[])
            .unwrap()
            .task;
        store
            .update(&owner, &run.run_id, State::Running, None, None, None)
            .unwrap();
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({"type":"RUN_STARTED","threadId":"thread-1","runId":"pending-run"}),
            )
            .unwrap();
        let result = TaskResult {
            text: "private candidate".into(),
            output: Some(json!({"answer":42})),
            attachments: Vec::new(),
            chat_id: None,
            message_id: Some("assistant-pending".into()),
        };
        let candidate = store
            .with_protocol_commit_permit(|permit| {
                store.prepare_protocol_terminal(
                    permit,
                    &owner,
                    &run.run_id,
                    State::Succeeded,
                    Some(result.clone()),
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(candidate.state, State::Succeeded);
        assert!(matches!(
            store.get(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.get_by_request(&owner, "pending-run"),
            Err(AppError::Io(_))
        ));
        assert!(matches!(store.list(&owner, None, 10), Err(AppError::Io(_))));
        assert!(store.protocol_events(&owner, "pending-run", 0).is_err());
        assert!(!store.lease_valid(&owner, &run.run_id));

        store
            .with_protocol_commit_permit(|permit| {
                store.append_protocol_commit_event(
                    permit,
                    &owner,
                    &run.run_id,
                    json!({"type":"TEXT_MESSAGE_START","messageId":"assistant-pending","role":"assistant"}),
                )?;
                store.append_protocol_commit_event(
                    permit,
                    &owner,
                    &run.run_id,
                    json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"assistant-pending","delta":"private candidate"}),
                )?;
                store.append_protocol_commit_event(
                    permit,
                    &owner,
                    &run.run_id,
                    json!({"type":"TEXT_MESSAGE_END","messageId":"assistant-pending"}),
                )?;
                store.append_protocol_commit_event(
                    permit,
                    &owner,
                    &run.run_id,
                    json!({
                        "type":"RUN_FINISHED","threadId":"thread-1","runId":"pending-run",
                        "outcome":{"type":"success"},"result":{"answer":42}
                    }),
                )?;
                Ok::<_, AppError>(())
            })
            .unwrap()
            .unwrap();
        let path = store.event_path(&run.run_id).unwrap();
        let event_bytes = fs::read(&path).unwrap();
        drop(store);

        let recovered = AgentStore::open(root.path()).unwrap();
        let task = recovered.get_by_request(&owner, "pending-run").unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert_eq!(
            task.error.as_ref().unwrap().code,
            PROTOCOL_COMMIT_INCOMPLETE
        );
        assert_eq!(fs::read(&path).unwrap(), event_bytes);
        assert!(recovered.protocol_events(&owner, "pending-run", 0).is_err());
        let duplicate = recovered.admit(&owner, &input("pending-run"), &[]).unwrap();
        assert!(!duplicate.start);
        assert_eq!(duplicate.task.run_id, run.run_id);
        let ledger = recovered.lock_ledger().unwrap();
        let record = &ledger.runs[&run.run_id];
        assert!(record.protocol_commit_pending.is_none());
        assert_eq!(
            record
                .protocol_candidate_evidence
                .as_ref()
                .and_then(|task| task.result.as_ref()),
            Some(&result)
        );
    }

    #[test]
    fn protocol_success_after_authorization_revocation_commits_only_cancelled_outcome() {
        let (_root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("revoked-protocol-success"), &[])
            .unwrap()
            .task;
        store
            .update(&owner, &run.run_id, State::Running, None, None, None)
            .unwrap();
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"thread-1", "runId":"revoked-protocol-success"
                }),
            )
            .unwrap();
        store.configure(owner.clone(), None).unwrap();
        let task = commit_protocol_terminal_for_test(
            &store,
            &owner,
            &run.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "must remain private".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: None,
                message_id: Some("revoked-assistant".into()),
            }),
            None,
        )
        .unwrap();

        assert_eq!(task.state, State::Cancelled);
        assert!(task.result.is_none());
        assert_eq!(task.error.as_ref().unwrap().code, "authorization_revoked");
        let events = store
            .protocol_events(&owner, "revoked-protocol-success", 0)
            .unwrap();
        assert!(events.events.iter().all(|event| {
            !matches!(
                event.get("type").and_then(Value::as_str),
                Some("TEXT_MESSAGE_START" | "TEXT_MESSAGE_CONTENT" | "TEXT_MESSAGE_END")
            )
        }));
        assert_eq!(
            events.events.last().unwrap()["outcome"]["type"],
            "cancelled"
        );
        assert!(store.committed_protocol_assistants().unwrap().is_empty());
    }

    #[test]
    fn prepare_rename_failure_recovers_as_hidden_pending_evidence() {
        let (root, store, owner) = setup();
        let run = store
            .admit(&owner, &input("prepare-rename-failure"), &[])
            .unwrap()
            .task;
        store
            .update(&owner, &run.run_id, State::Running, None, None, None)
            .unwrap();
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"thread-1", "runId":"prepare-rename-failure"
                }),
            )
            .unwrap();
        let event_path = store.event_path(&run.run_id).unwrap();
        let started_bytes = fs::read(&event_path).unwrap();
        let candidate_result = TaskResult {
            text: "private prepared answer".into(),
            output: None,
            attachments: Vec::new(),
            chat_id: None,
            message_id: Some("assistant-prepare-failure".into()),
        };
        store
            .fail_next_persist_after_rename
            .store(true, Ordering::Release);
        let result = store.with_protocol_commit_permit(|permit| {
            store.prepare_protocol_terminal(
                permit,
                &owner,
                &run.run_id,
                State::Succeeded,
                Some(candidate_result.clone()),
                None,
                None,
            )
        });
        assert!(matches!(result, Ok(Err(AppError::Io(_)))));
        assert!(matches!(
            store.get_by_request(&owner, "prepare-rename-failure"),
            Err(AppError::Io(_))
        ));
        assert_eq!(fs::read(&event_path).unwrap(), started_bytes);
        drop(store);

        let recovered = AgentStore::open(root.path()).unwrap();
        let task = recovered
            .get_by_request(&owner, "prepare-rename-failure")
            .unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert_eq!(
            task.error.as_ref().unwrap().code,
            PROTOCOL_COMMIT_INCOMPLETE
        );
        assert_eq!(fs::read(&event_path).unwrap(), started_bytes);
        assert!(recovered
            .protocol_events(&owner, "prepare-rename-failure", 0)
            .is_err());
        let evidence = recovered.lock_ledger().unwrap();
        assert_eq!(
            evidence.runs[&run.run_id]
                .protocol_candidate_evidence
                .as_ref()
                .and_then(|task| task.result.as_ref()),
            Some(&candidate_result)
        );
    }

    #[test]
    fn event_sync_failure_keeps_pending_candidate_and_exact_event_bytes() {
        let (root, store, owner) = setup();
        let request_id = "event-sync-failure";
        let run = store.admit(&owner, &input(request_id), &[]).unwrap().task;
        store
            .update(&owner, &run.run_id, State::Running, None, None, None)
            .unwrap();
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"thread-1", "runId":request_id
                }),
            )
            .unwrap();
        let event_path = store.event_path(&run.run_id).unwrap();
        let candidate_result = TaskResult {
            text: "private answer".into(),
            output: None,
            attachments: Vec::new(),
            chat_id: None,
            message_id: Some("assistant-sync-failure".into()),
        };
        let result = store.with_protocol_commit_permit(|permit| {
            store.prepare_protocol_terminal(
                permit,
                &owner,
                &run.run_id,
                State::Succeeded,
                Some(candidate_result.clone()),
                None,
                None,
            )?;
            for event in [
                json!({"type":"TEXT_MESSAGE_START","messageId":"assistant-sync-failure","role":"assistant"}),
                json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"assistant-sync-failure","delta":"private answer"}),
                json!({"type":"TEXT_MESSAGE_END","messageId":"assistant-sync-failure"}),
            ] {
                store.append_protocol_commit_event(permit, &owner, &run.run_id, event)?;
            }
            store.fail_next_event_sync.store(true, Ordering::Release);
            store.append_protocol_commit_event(
                permit,
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_FINISHED", "threadId":"thread-1", "runId":request_id,
                    "outcome":{"type":"success"}
                }),
            )?;
            store.finalize_protocol_terminal(permit, &owner, &run.run_id)
        });
        assert!(matches!(result, Ok(Err(AppError::Io(_)))));
        assert!(matches!(
            store.get_by_request(&owner, request_id),
            Err(AppError::Io(_))
        ));
        let event_bytes = fs::read(&event_path).unwrap();
        drop(store);

        let recovered = AgentStore::open(root.path()).unwrap();
        let task = recovered.get_by_request(&owner, request_id).unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert_eq!(
            task.error.as_ref().unwrap().code,
            PROTOCOL_COMMIT_INCOMPLETE
        );
        assert_eq!(fs::read(&event_path).unwrap(), event_bytes);
        assert!(recovered.protocol_events(&owner, request_id, 0).is_err());
        assert!(
            !recovered
                .admit(&owner, &input(request_id), &[])
                .unwrap()
                .start
        );
    }

    #[test]
    fn final_ledger_rename_failure_is_quarantined_and_reopens_consistent_commit() {
        let (root, store, owner) = setup();
        let request_id = "final-rename-failure";
        let run = store.admit(&owner, &input(request_id), &[]).unwrap().task;
        store
            .update(&owner, &run.run_id, State::Running, None, None, None)
            .unwrap();
        store
            .append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"thread-1", "runId":request_id
                }),
            )
            .unwrap();
        let result = TaskResult {
            text: "committed answer".into(),
            output: None,
            attachments: Vec::new(),
            chat_id: None,
            message_id: Some("assistant-final-failure".into()),
        };
        let event_path = store.event_path(&run.run_id).unwrap();
        let outcome = store.with_protocol_commit_permit(|permit| {
            let _candidate = store.prepare_protocol_terminal(
                permit,
                &owner,
                &run.run_id,
                State::Succeeded,
                Some(result.clone()),
                None,
                None,
            )?;
            for event in [
                json!({"type":"TEXT_MESSAGE_START","messageId":"assistant-final-failure","role":"assistant"}),
                json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"assistant-final-failure","delta":"committed answer"}),
                json!({"type":"TEXT_MESSAGE_END","messageId":"assistant-final-failure"}),
                json!({
                    "type":"RUN_FINISHED", "threadId":"thread-1", "runId":request_id,
                    "outcome":{"type":"success"}
                }),
            ] {
                store.append_protocol_commit_event(permit, &owner, &run.run_id, event)?;
            }
            store
                .fail_next_persist_after_rename
                .store(true, Ordering::Release);
            store.finalize_protocol_terminal(permit, &owner, &run.run_id)
        });
        assert!(matches!(outcome, Ok(Err(AppError::Io(_)))));
        assert!(matches!(
            store.get_by_request(&owner, request_id),
            Err(AppError::Io(_))
        ));
        let committed_event_bytes = fs::read(&event_path).unwrap();
        drop(store);

        let recovered = AgentStore::open(root.path()).unwrap();
        let task = recovered.get_by_request(&owner, request_id).unwrap();
        assert_eq!(task.state, State::Succeeded);
        assert_eq!(task.result, Some(result));
        let page = recovered.protocol_events(&owner, request_id, 0).unwrap();
        assert_eq!(page.events.last().unwrap()["type"], "RUN_FINISHED");
        assert_eq!(fs::read(&event_path).unwrap(), committed_event_bytes);
        assert!(
            !recovered
                .admit(&owner, &input(request_id), &[])
                .unwrap()
                .start
        );
    }
}

#[cfg(test)]
mod background_expiration_tests {
    use super::protocol_failure_tests::{input, setup};
    use super::*;
    use std::sync::{Arc, Barrier};

    fn start(store: &AgentStore, owner: &Owner, request_id: &str) -> Task {
        let task = store.admit(owner, &input(request_id), &[]).unwrap().task;
        store
            .append_event(
                owner,
                &task.run_id,
                json!({
                    "type":"RUN_STARTED", "threadId":"thread-1", "runId":request_id
                }),
            )
            .unwrap();
        store
            .update(owner, &task.run_id, State::Running, None, None, None)
            .unwrap()
    }

    fn result() -> TaskResult {
        TaskResult {
            text: "completed".into(),
            output: None,
            attachments: vec![],
            chat_id: None,
            message_id: Some("assistant-result".into()),
        }
    }

    fn commit_cancel(store: &AgentStore, owner: &Owner, id: &str) -> Task {
        commit_protocol_terminal_for_test(
            store,
            owner,
            id,
            State::Cancelled,
            None,
            Some("cancelled".into()),
        )
        .unwrap()
    }

    #[test]
    fn expiration_waits_for_reap_and_commits_an_interrupted_terminal() {
        let (root, store, owner) = setup();
        let run = start(&store, &owner, "expiry-run");
        store
            .request_background_expiration(&owner, &run.run_id)
            .unwrap();
        assert!(!store.lease_valid(&owner, &run.run_id));
        assert_eq!(
            store.get(&owner, &run.run_id).unwrap().state,
            State::Running
        );
        assert!(matches!(
            store.admit(&owner, &input("other-run"), &[]),
            Err(AppError::Busy(_))
        ));

        let waiting = store
            .update(
                &owner,
                &run.run_id,
                State::Running,
                None,
                None,
                Some("tool_cleanup_incomplete".into()),
            )
            .unwrap();
        assert_eq!(waiting.state, State::Running);
        assert!(matches!(
            store.admit(&owner, &input("other-run"), &[]),
            Err(AppError::Busy(_))
        ));

        let task = commit_cancel(&store, &owner, &run.run_id);
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.error.as_ref().unwrap().code, IOS_BACKGROUND_EXPIRED);
        drop(store);
        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened.get_by_request(&owner, "expiry-run").unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.error.as_ref().unwrap().code, IOS_BACKGROUND_EXPIRED);
        let page = reopened.protocol_events(&owner, "expiry-run", 0).unwrap();
        let terminal = page.events.last().unwrap();
        assert_eq!(terminal["type"], "RUN_ERROR");
        assert_eq!(terminal["code"], IOS_BACKGROUND_EXPIRED);
        assert_eq!(terminal["message"], error_message(IOS_BACKGROUND_EXPIRED));
        assert!(
            !reopened
                .admit(&owner, &input("expiry-run"), &[])
                .unwrap()
                .start
        );
        assert!(
            reopened
                .admit(&owner, &input("other-run"), &[])
                .unwrap()
                .start
        );
    }

    #[test]
    fn restart_uses_only_a_persisted_expiration_reason() {
        for expired in [false, true] {
            let (root, store, owner) = setup();
            let run = start(&store, &owner, "restart-run");
            if expired {
                store
                    .request_background_expiration(&owner, &run.run_id)
                    .unwrap();
            }
            drop(store);
            let reopened = AgentStore::open(root.path()).unwrap();
            let task = reopened.get_by_request(&owner, "restart-run").unwrap();
            let expected = if expired {
                IOS_BACKGROUND_EXPIRED
            } else {
                "host_restarted"
            };
            assert_eq!(task.state, State::Interrupted);
            assert_eq!(task.error.as_ref().unwrap().code, expected);
            let page = reopened.protocol_events(&owner, "restart-run", 0).unwrap();
            let terminal = page.events.last().unwrap();
            assert_eq!(terminal["type"], "RUN_ERROR");
            assert_eq!(terminal["code"], expected);
            assert_eq!(terminal["message"], task.error.unwrap().message);
            assert!(
                !reopened
                    .admit(&owner, &input("restart-run"), &[])
                    .unwrap()
                    .start
            );
        }
    }

    #[test]
    fn prior_cancel_revoke_or_terminal_state_is_not_relabelled() {
        for prior in ["cancel", "revoke", "finished"] {
            let (_root, store, owner) = setup();
            let run = start(&store, &owner, "prior-run");
            match prior {
                "cancel" => store.request_cancel(&owner, &run.run_id).unwrap(),
                "revoke" => {
                    store.configure(owner.clone(), None).unwrap();
                }
                _ => {
                    commit_protocol_terminal_for_test(
                        &store,
                        &owner,
                        &run.run_id,
                        State::Succeeded,
                        Some(result()),
                        None,
                    )
                    .unwrap();
                }
            }
            let before = fs::read(&store.path).unwrap();
            store
                .request_background_expiration(&owner, &run.run_id)
                .unwrap();
            assert_eq!(fs::read(&store.path).unwrap(), before);
            let ledger = store.lock_ledger().unwrap();
            assert_eq!(ledger.runs[&run.run_id].stop_intent, None);
            drop(ledger);
            if prior == "finished" {
                assert_eq!(
                    store.get(&owner, &run.run_id).unwrap().state,
                    State::Succeeded
                );
            } else {
                let task = commit_cancel(&store, &owner, &run.run_id);
                assert_eq!(task.state, State::Cancelled);
                assert_ne!(task.error.unwrap().code, IOS_BACKGROUND_EXPIRED);
            }
        }
    }

    #[test]
    fn expiry_and_success_are_linearized_before_any_terminal_event() {
        for _ in 0..12 {
            let (_root, store, owner) = setup();
            let store = Arc::new(store);
            let run = start(&store, &owner, "race-run");
            let barrier = Arc::new(Barrier::new(3));
            let expiry_store = store.clone();
            let expiry_owner = owner.clone();
            let expiry_id = run.run_id.clone();
            let expiry_barrier = barrier.clone();
            let expiry = std::thread::spawn(move || {
                expiry_barrier.wait();
                expiry_store.request_background_expiration(&expiry_owner, &expiry_id)
            });
            let finish_store = store.clone();
            let finish_owner = owner.clone();
            let finish_id = run.run_id.clone();
            let finish_barrier = barrier.clone();
            let finish = std::thread::spawn(move || {
                finish_barrier.wait();
                commit_protocol_terminal_for_test(
                    &finish_store,
                    &finish_owner,
                    &finish_id,
                    State::Succeeded,
                    Some(result()),
                    None,
                )
                .unwrap()
            });
            barrier.wait();
            expiry.join().unwrap().unwrap();
            let task = finish.join().unwrap();
            let ledger = store.lock_ledger().unwrap();
            let intent = ledger.runs[&run.run_id].stop_intent.clone();
            drop(ledger);
            if task.state == State::Succeeded {
                assert_eq!(intent, None);
                assert!(task.error.is_none());
            } else {
                assert_eq!(task.state, State::Interrupted);
                assert_eq!(intent, Some(StopIntent::IosBackgroundExpired));
                assert_eq!(task.error.unwrap().code, IOS_BACKGROUND_EXPIRED);
            }
            assert!(store.protocol_events(&owner, "race-run", 0).is_ok());
        }
    }

    #[test]
    fn expiry_races_with_cancel_and_revoke_without_relabelling_an_earlier_intent() {
        for operation in ["cancel", "revoke"] {
            for _ in 0..12 {
                let (_root, store, owner) = setup();
                let store = Arc::new(store);
                let run = start(&store, &owner, "race-run");
                let barrier = Arc::new(Barrier::new(3));
                let expiry_store = store.clone();
                let expiry_owner = owner.clone();
                let expiry_id = run.run_id.clone();
                let expiry_barrier = barrier.clone();
                let expiry = std::thread::spawn(move || {
                    expiry_barrier.wait();
                    expiry_store.request_background_expiration(&expiry_owner, &expiry_id)
                });
                let stop_store = store.clone();
                let stop_owner = owner.clone();
                let stop_id = run.run_id.clone();
                let stop_barrier = barrier.clone();
                let stop = std::thread::spawn(move || {
                    stop_barrier.wait();
                    if operation == "cancel" {
                        stop_store.request_cancel(&stop_owner, &stop_id)
                    } else {
                        stop_store.configure(stop_owner, None).map(|_| ())
                    }
                });
                barrier.wait();
                expiry.join().unwrap().unwrap();
                stop.join().unwrap().unwrap();
                let intent = store.lock_ledger().unwrap().runs[&run.run_id]
                    .stop_intent
                    .clone();
                let task = commit_cancel(&store, &owner, &run.run_id);
                if intent == Some(StopIntent::IosBackgroundExpired) {
                    assert_eq!(task.state, State::Interrupted);
                    assert_eq!(task.error.unwrap().code, IOS_BACKGROUND_EXPIRED);
                } else {
                    assert_eq!(task.state, State::Cancelled);
                    assert_eq!(task.error.unwrap().code, "cancelled");
                }
            }
        }
    }

    #[test]
    fn failed_expiry_after_rename_fences_all_store_access_until_reopen() {
        let (root, store, owner) = setup();
        let store = Arc::new(store);
        let run = start(&store, &owner, "uncertain-expiry");
        let persist_barrier = Arc::new(Barrier::new(2));
        *store.persist_failure_barrier.lock().unwrap() = Some(persist_barrier.clone());
        store
            .fail_next_persist_after_rename
            .store(true, Ordering::Release);
        let expiry_store = store.clone();
        let expiry_owner = owner.clone();
        let expiry_id = run.run_id.clone();
        let expiry = std::thread::spawn(move || {
            expiry_store.request_background_expiration(&expiry_owner, &expiry_id)
        });
        // The expiration write has renamed its ledger while retaining both
        // locks. Queue a reader, writer and admission before it reports Err.
        persist_barrier.wait();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader_store = store.clone();
        let reader_owner = owner.clone();
        let reader_ready = ready_tx.clone();
        let reader = std::thread::spawn(move || {
            reader_ready.send(()).unwrap();
            reader_store.get_by_request(&reader_owner, "uncertain-expiry")
        });
        let writer_store = store.clone();
        let writer_owner = owner.clone();
        let writer_id = run.run_id.clone();
        let writer_ready = ready_tx.clone();
        let writer = std::thread::spawn(move || {
            writer_ready.send(()).unwrap();
            writer_store.set_phase(&writer_owner, &writer_id, Some("waiting_for_answer"))
        });
        let admission_store = store.clone();
        let admission_owner = owner.clone();
        let admission = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            admission_store.admit(&admission_owner, &input("concurrent-run"), &[])
        });
        for _ in 0..3 {
            ready_rx.recv().unwrap();
        }
        persist_barrier.wait();
        assert!(matches!(expiry.join().unwrap(), Err(AppError::Io(_))));
        assert!(matches!(reader.join().unwrap(), Err(AppError::Io(_))));
        assert!(matches!(writer.join().unwrap(), Err(AppError::Io(_))));
        assert!(matches!(admission.join().unwrap(), Err(AppError::Io(_))));
        let disk = fs::read(&store.path).unwrap();
        let disk_ledger: Ledger = serde_json::from_slice(&disk).unwrap();
        assert_eq!(
            disk_ledger.runs[&run.run_id].stop_intent,
            Some(StopIntent::IosBackgroundExpired)
        );
        assert!(disk_ledger.runs[&run.run_id].cancel_requested);

        assert!(matches!(store.settings(&owner), Err(AppError::Io(_))));
        assert!(matches!(
            store.get(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.get_by_request(&owner, "uncertain-expiry"),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.task_id_for_request(&owner, "uncertain-expiry"),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.thread_id_for_request(&owner, "uncertain-expiry"),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.protocol_input_fingerprint(&owner, "uncertain-expiry"),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.protocol_events(&owner, "uncertain-expiry", 0),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.recovery_states(&owner.app_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(store.list(&owner, None, 10), Err(AppError::Io(_))));
        assert!(matches!(
            store.native_output_bytes_for_request(&owner, "uncertain-expiry", 1),
            Err(AppError::Io(_))
        ));
        assert!(!store.lease_valid(&owner, &run.run_id));
        assert!(matches!(
            store.set_phase(&owner, &run.run_id, Some("waiting_for_answer")),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.record_native_output_bytes(&owner, &run.run_id, 1, 1),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.set_completed_model_turns(&owner, &run.run_id, 1),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.request_cancel(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.request_background_expiration(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.configure(owner.clone(), None),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.update(&owner, &run.run_id, State::Cancelled, None, None, None),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.admit(&owner, &input("other-run"), &[]),
            Err(AppError::Io(_))
        ));
        assert!(matches!(
            store.append_event(
                &owner,
                &run.run_id,
                json!({
                    "type":"RUN_ERROR", "message":"must not append"
                })
            ),
            Err(AppError::Io(_))
        ));
        let called = std::cell::Cell::new(false);
        assert!(matches!(
            store.with_protocol_commit(|| called.set(true)),
            Err(AppError::Io(_))
        ));
        assert!(!called.get());

        assert_eq!(fs::read(&store.path).unwrap(), disk);
        drop(store);

        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened.get_by_request(&owner, "uncertain-expiry").unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.error.unwrap().code, IOS_BACKGROUND_EXPIRED);
        let page = reopened
            .protocol_events(&owner, "uncertain-expiry", 0)
            .unwrap();
        assert_eq!(page.events.last().unwrap()["code"], IOS_BACKGROUND_EXPIRED);
        assert!(
            !reopened
                .admit(&owner, &input("uncertain-expiry"), &[])
                .unwrap()
                .start
        );
        assert!(
            reopened
                .admit(&owner, &input("other-run"), &[])
                .unwrap()
                .start
        );
    }

    #[test]
    fn failed_expiry_before_rename_reopens_with_unknown_reason() {
        let (root, mut store, owner) = setup();
        let run = start(&store, &owner, "unpersisted-expiry");
        let ledger_path = store.path.clone();
        let original = fs::read(&ledger_path).unwrap();
        let blocked = ledger_path.with_extension("unwritable-directory");
        fs::create_dir(&blocked).unwrap();
        store.path = blocked;
        assert!(store
            .request_background_expiration(&owner, &run.run_id)
            .is_err());
        assert!(matches!(
            store.get(&owner, &run.run_id),
            Err(AppError::Io(_))
        ));
        assert!(!store.lease_valid(&owner, &run.run_id));
        assert_eq!(fs::read(&ledger_path).unwrap(), original);
        store.path = ledger_path;
        drop(store);
        let reopened = AgentStore::open(root.path()).unwrap();
        let task = reopened
            .get_by_request(&owner, "unpersisted-expiry")
            .unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.error.unwrap().code, "host_restarted");
        let page = reopened
            .protocol_events(&owner, "unpersisted-expiry", 0)
            .unwrap();
        assert_eq!(page.events.last().unwrap()["code"], "host_restarted");
    }
}
