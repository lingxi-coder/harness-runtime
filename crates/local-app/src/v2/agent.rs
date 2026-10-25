//! Durable app-task admission. Native hosts derive Owner from the active run,
//! never from a page message. Prompt/images/credentials are deliberately absent
//! from persisted records. All methods are synchronous; call off the UI thread.
mod chat_store;
pub mod protocol;
pub use chat_store::{ChatMessageMetadata, ChatStore};

use super::AppError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_ATTACHMENT_BASE64_BYTES: usize = ((MAX_ATTACHMENT_BYTES + 2) / 3) * 4;
const MAX_PROTOCOL_EVENT_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROTOCOL_EVENT_PAGE: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Owner {
    pub app_id: String,
    pub release_digest: String,
    pub invocation_id: String,
}
impl Owner {
    pub fn validate(&self) -> Result<(), AppError> {
        let hex = |s: &str, n| {
            s.len() == n
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        };
        if !self.app_id.starts_with('a')
            || !hex(&self.app_id[1..], 32)
            || !hex(&self.release_digest, 64)
            || self.invocation_id.len() > 128
            || self.invocation_id.is_empty()
            || self.invocation_id.chars().any(char::is_control)
        {
            return Err(AppError::Invalid("invalid Agent task owner".into()));
        }
        Ok(())
    }
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
    pub prompt: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Ordered normalized parts used by the AG-UI adapter. The older private
    /// prompt/attachments form remains an internal boundary input only.
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Observed Harness end-turn callbacks; a count, not a completion percent.
    pub completed_model_turns: u32,
    /// Configured turn ceiling, not an estimate of total work.
    pub turn_limit: u32,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatMessage {
    pub message_id: String,
    pub run_id: String,
    pub role: ChatRole,
    pub text: String,
    pub created_at: u64,
    pub attachments: Vec<AttachmentMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatHistory {
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Grant {
    owner: Owner,
    settings: Settings,
    epoch: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    owner: Owner,
    input_digest: String,
    epoch: u64,
    cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_input_digest: Option<String>,
    task: Task,
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
}

/// One instance per native app service. Its caller owns the library RuntimeLease.
pub struct AgentStore {
    path: PathBuf,
    events_root: PathBuf,
    ledger: Mutex<Ledger>,
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
        "invalid_images" | "invalid_attachments" => {
            "A file could not be decoded or exceeds the attachment limits. Choose a smaller valid file."
        }
        "attachment_type_unsupported" => {
            "This file type is not supported. Use a text document, PDF or supported image format."
        }
        "skill_unavailable" | "skill_integrity_failed" => {
            "A selected release Skill file is unavailable or failed integrity verification. Publish a new release or choose another Skill."
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
        "mcp_definition_changed" => {
            "The MCP configuration or tool definition changed. Review and authorize the current tool again."
        }
        "tool_cleanup_incomplete" => {
            "A tool has not finished stopping. The host is retaining the task's resources until cleanup completes."
        }
        "agent_storage_unavailable" | "task_storage_failed" => {
            "Agent task storage is unavailable. Existing task data has been preserved."
        }
        _ => "The Agent task could not complete. Review the host status before retrying.",
    }
}

impl AgentStore {
    pub fn open(root: &Path) -> Result<Self, AppError> {
        ensure_private_directory(root)?;
        let events_root = root.join("protocol-events");
        ensure_private_directory(&events_root)?;
        let path = root.join("agent-tasks.json");
        let mut ledger: Ledger = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => return Err(e.into()),
        };
        for record in ledger.runs.values_mut() {
            if !record.task.state.terminal() {
                record.task.state = State::Interrupted;
                record.task.phase = None;
                record.task.error = Some(TaskError {
                    code: "host_restarted".into(),
                    message:
                        "The host restarted. This task was interrupted and will not be replayed."
                            .into(),
                });
                record.task.sequence += 1;
                record.task.updated_at = now();
            } else if record.protocol_thread_id.is_some()
                && !read_protocol_events(&protocol_event_path(&events_root, &record.task.run_id)?)?
                    .last()
                    .is_some_and(|entry| is_terminal_event(&entry.event))
                && record.task.state == State::Succeeded
            {
                record.task.state = State::Interrupted;
                record.task.error = Some(TaskError {
                    code: "protocol_terminal_missing".into(),
                    message: "The run completed without a durable terminal protocol event.".into(),
                });
                record.task.sequence += 1;
                record.task.updated_at = now();
            }
        }
        let store = Self {
            path,
            events_root,
            ledger: Mutex::new(ledger),
        };
        {
            let ledger = store
                .ledger
                .lock()
                .map_err(|_| invalid("Agent storage unavailable"))?;
            store.persist(&ledger)?;
        }
        let incomplete = store
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?
            .runs
            .values()
            .filter(|record| record.protocol_thread_id.is_some())
            .map(|record| {
                (
                    record.owner.clone(),
                    record.task.run_id.clone(),
                    record.task.request_id.clone(),
                    record
                        .protocol_thread_id
                        .clone()
                        .expect("filtered thread ID"),
                    record.task.state.clone(),
                    record.task.error.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (owner, task_id, run_id, thread_id, state, task_error) in incomplete {
            let path = store.event_path(&task_id)?;
            let events = read_protocol_events(&path)?;
            if events.is_empty() {
                store.append_event(
                    &owner,
                    &task_id,
                    json!({
                        "type":"RUN_STARTED",
                        "protocolVersion":"1.0",
                        "threadId":thread_id,
                        "runId":run_id
                    }),
                )?;
            }
            if !events
                .last()
                .is_some_and(|entry| is_terminal_event(&entry.event))
            {
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
                            "message":"The host restarted before the Agent run completed.",
                            "code":task_error.as_ref().map_or("host_restarted", |error| error.code.as_str())
                        })
                    }
                };
                store.append_event(&owner, &task_id, terminal)?;
            }
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
            fs::File::open(
                self.path
                    .parent()
                    .ok_or_else(|| invalid("Agent storage unavailable"))?,
            )?
            .sync_all()?;
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(temp);
        }
        write
    }
    fn change<T>(
        &self,
        edit: impl FnOnce(&mut Ledger) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let mut next = ledger.clone();
        let result = edit(&mut next)?;
        self.persist(&next)?;
        *ledger = next;
        Ok(result)
    }
    /// Trusted native settings UI only. A changed grant invalidates every task
    /// lease immediately; the executor must cancel and reap returned run IDs.
    pub fn configure(
        &self,
        owner: Owner,
        settings: Option<Settings>,
    ) -> Result<Vec<String>, AppError> {
        owner.validate()?;
        if let Some(s) = &settings {
            s.validate()?;
        }
        self.change(|ledger| {
            ledger.epoch = ledger
                .epoch
                .checked_add(1)
                .ok_or_else(|| invalid("Agent grant generation exhausted"))?;
            let epoch = ledger.epoch;
            match settings {
                Some(settings) => {
                    ledger.grants.insert(
                        owner.app_id.clone(),
                        Grant {
                            owner: owner.clone(),
                            settings,
                            epoch,
                        },
                    );
                }
                None => {
                    ledger.grants.remove(&owner.app_id);
                }
            }
            Ok(ledger
                .runs
                .values()
                .filter(|r| r.owner.app_id == owner.app_id && !r.task.state.terminal())
                .map(|r| r.task.run_id.clone())
                .collect())
        })
    }
    pub fn settings(&self, owner: &Owner) -> Result<Option<Settings>, AppError> {
        owner.validate()?;
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
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
        owner.validate()?;
        validate_input(input)?;
        if chat_id
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        {
            return Err(invalid("invalid Chat ID"));
        }
        let hash = digest(input, chat_id)?;
        self.change(|ledger| {
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
                    chat_id: chat_id.map(str::to_owned),
                    protocol_thread_id: input.protocol_thread_id.clone(),
                    protocol_input_digest: input.protocol_input_digest.clone(),
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
        owner.validate()?;
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let record = ledger
            .runs
            .get(id)
            .filter(|r| {
                r.owner.app_id == owner.app_id && r.owner.release_digest == owner.release_digest
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        Ok(record.task.clone())
    }
    /// Resolve the public AG-UI runId to the private, path-safe task UUID.
    pub fn task_id_for_request(&self, owner: &Owner, run_id: &str) -> Result<String, AppError> {
        owner.validate()?;
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .map(|record| record.task.run_id.clone())
            .ok_or_else(|| AppError::NotFound("Agent task".into()))
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
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .and_then(|record| record.protocol_input_digest.clone())
            .ok_or_else(|| AppError::NotFound("AG-UI request fingerprint".into()))
    }
    pub fn thread_id_for_request(&self, owner: &Owner, run_id: &str) -> Result<String, AppError> {
        owner.validate()?;
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .and_then(|record| record.protocol_thread_id.clone())
            .ok_or_else(|| AppError::NotFound("AG-UI task".into()))
    }
    /// Remove an admission that failed before RUN_STARTED. The request ID can
    /// then be retried because the host never accepted execution.
    pub fn rollback_queued_admission(&self, owner: &Owner, id: &str) -> Result<(), AppError> {
        owner.validate()?;
        let path = self.event_path(id)?;
        let mut ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let record = ledger
            .runs
            .get(id)
            .filter(|record| record.owner == *owner)
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        if record.task.state != State::Queued || !read_protocol_events(&path)?.is_empty() {
            return Err(invalid("Agent admission has already started"));
        }
        let mut next = ledger.clone();
        next.runs.remove(id);
        self.persist(&next)?;
        *ledger = next;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    /// Append one schema-validated AG-UI event to the durable task log.
    pub fn append_event(
        &self,
        owner: &Owner,
        task_id: &str,
        event: Value,
    ) -> Result<u64, AppError> {
        owner.validate()?;
        protocol::validate_event(&event)
            .map_err(|_| invalid("AG-UI event does not match the pinned protocol"))?;
        let path = self.event_path(task_id)?;
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let record = ledger
            .runs
            .get(task_id)
            .filter(|record| record.owner == *owner)
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        let thread_id = record
            .protocol_thread_id
            .as_deref()
            .ok_or_else(|| invalid("Agent task is not an AG-UI run"))?;
        validate_event_identity(&event, thread_id, &record.task.request_id)?;
        let events = read_protocol_events(&path)?;
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
        let mut file = options.open(&path)?;
        use std::io::Write;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
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
        owner.validate()?;
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let record = ledger
            .runs
            .values()
            .find(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
                    && record.task.request_id == run_id
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        if record.protocol_thread_id.is_none() {
            return Err(invalid("Agent task is not an AG-UI run"));
        }
        let task_id = record.task.run_id.clone();
        let path = self.event_path(&task_id)?;
        let events = read_protocol_events(&path)?;
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
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
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
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Agent storage unavailable"))?;
        let mut tasks = ledger
            .runs
            .values()
            .filter(|record| {
                record.owner.app_id == owner.app_id
                    && record.owner.release_digest == owner.release_digest
            })
            .map(|record| record.task.clone())
            .collect::<Vec<_>>();
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
        self.ledger.lock().ok().is_some_and(|ledger| {
            ledger.runs.get(id).is_some_and(|r| {
                r.owner == *owner
                    && !r.cancel_requested
                    && !r.task.state.terminal()
                    && ledger.grants.get(&owner.app_id).is_some_and(|g| {
                        g.epoch == r.epoch && g.owner.release_digest == owner.release_digest
                    })
            })
        })
    }
    /// Cancel intent is durable; admission stays occupied until foreground reap.
    pub fn request_cancel(&self, owner: &Owner, id: &str) -> Result<(), AppError> {
        self.change(|ledger| {
            let record = ledger
                .runs
                .get_mut(id)
                .filter(|r| {
                    r.owner.app_id == owner.app_id && r.owner.release_digest == owner.release_digest
                })
                .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
            if !record.task.state.terminal() {
                record.cancel_requested = true;
            }
            Ok(())
        })
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
            if !record.task.state.terminal() && record.task.phase.as_deref() != phase {
                record.task.phase = phase.map(String::from);
                record.task.sequence += 1;
                record.task.updated_at = now();
            }
            Ok(())
        })
    }
    /// Publish a real completed-turn count from the Harness end-turn callback.
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
            if record.task.state.terminal() {
                return Ok(record.task.clone());
            }
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
            record.task.state = state;
            record.task.phase = None;
            record.task.sequence += 1;
            record.task.updated_at = now();
            record.task.result = result;
            record.task.usage = usage;
            record.task.error = error.map(|code| TaskError {
                message: error_message(&code).into(),
                code,
            });
            Ok(record.task.clone())
        })
    }

    /// If the standard terminal event could not be durably recorded, prevent
    /// private task metadata from claiming a successful public run.
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
            record.task.state = State::Interrupted;
            record.task.phase = None;
            record.task.result = None;
            record.task.error = Some(TaskError {
                code: code.to_owned(),
                message: error_message(code).to_owned(),
            });
            record.task.sequence = record.task.sequence.saturating_add(1);
            record.task.updated_at = now();
            Ok(record.task.clone())
        })
    }
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

fn read_protocol_events(path: &Path) -> Result<Vec<SequencedEvent>, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PROTOCOL_EVENT_LOG_BYTES
    {
        return Err(invalid("AG-UI event log is unavailable"));
    }
    let encoded = fs::read_to_string(path)?;
    let mut events = Vec::new();
    for line in encoded.lines() {
        let entry: SequencedEvent =
            serde_json::from_str(line).map_err(|_| invalid("AG-UI event log is invalid"))?;
        if entry.sequence != events.len() as u64 + 1
            || protocol::validate_event(&entry.event).is_err()
            || (events.is_empty() && entry.event["type"] != "RUN_STARTED")
            || events
                .last()
                .is_some_and(|last: &SequencedEvent| is_terminal_event(&last.event))
        {
            return Err(invalid("AG-UI event log ordering is invalid"));
        }
        events.push(entry);
    }
    Ok(events)
}

#[cfg(test)]
mod tests;
