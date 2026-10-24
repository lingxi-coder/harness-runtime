//! Durable app-task admission. Native hosts derive Owner from the active run,
//! never from a page message. Prompt/images/credentials are deliberately absent
//! from persisted records. All methods are synchronous; call off the UI thread.
use super::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

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
    pub prompt: String,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub media_type: String,
    pub base64: String,
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
    pub created_at: u64,
    pub updated_at: u64,
    pub model: Model,
    pub limits: Limits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
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
    task: Task,
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
    ledger: Mutex<Ledger>,
}
/// Newly accepted tasks execute exactly once; duplicates only return a snapshot.
pub struct Admission {
    pub task: Task,
    pub start: bool,
    pub tools: BTreeMap<String, ToolGrant>,
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
fn digest(input: &Input) -> Result<String, AppError> {
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
        Sha256::digest(serde_json::to_vec(&canonical(serde_json::to_value(
            input
        )?))?)
    ))
}
/// Stable page-facing errors; storage details and raw OS errors stay in the host.
pub fn admission_error_code(error: &AppError) -> &'static str {
    match error {
        AppError::Busy(_) => "busy",
        AppError::Invalid(message) => match message.as_str() {
            "Agent is not authorized for this release" => "agent_not_authorized",
            "tool is not declared and granted" => "tool_not_authorized",
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
        "agent_not_authorized" => "Enable Agent access and select a model in this application's native settings.",
        "tool_not_authorized" => "The requested tool must be declared by the application and approved in native Agent settings.",
        "busy" => "This application already has an active Agent task. Wait for it or cancel it first.",
        "request_conflict" => "This request ID was already used with different input. Retry the original input or use a new request ID.",
        "stale_request" => "This request belongs to an earlier application invocation and cannot be started again.",
        "limits_exceeded" => "Requested task limits exceed the limits selected in native Agent settings.",
        "invalid_agent_input" => "The Agent request is invalid. Check its prompt, images, schema and limits.",
        "model_unavailable" | "model_credentials_unavailable" => "Configure an available model in this application's native Agent settings.",
        "model_vision_unsupported" => "The selected model does not support images. Select an image-capable model in native Agent settings.",
        "model_tools_unsupported" => "The selected model does not support the tools required for this task or structured result.",
        "invalid_images" => "The image could not be decoded or exceeds the image limits. Choose a smaller valid image.",
        "invalid_output_schema" | "unsupported_schema_dialect" => "The result schema must be a valid Draft-7 object with local references only.",
        "timeout_exceeded" => "The Agent task reached its time limit and was stopped.",
        "cancelled" | "authorization_revoked" => "The Agent task was cancelled or its authorization was revoked.",
        "mcp_definition_changed" => "The MCP configuration or tool definition changed. Review and authorize the current tool again.",
        "tool_cleanup_incomplete" => "A tool has not finished stopping. The host is retaining the task's resources until cleanup completes.",
        "agent_storage_unavailable" | "task_storage_failed" => "Agent task storage is unavailable. Existing task data has been preserved.",
        _ => "The Agent task could not complete. Review the host status before retrying.",
    }
}

impl AgentStore {
    pub fn open(root: &Path) -> Result<Self, AppError> {
        fs::create_dir_all(root)?;
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
            }
        }
        let store = Self {
            path,
            ledger: Mutex::new(ledger),
        };
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
        owner.validate()?;
        if input.request_id.is_empty()
            || input.request_id.len() > 128
            || input.request_id.chars().any(char::is_control)
            || input.prompt.trim().is_empty()
            || input.prompt.len() > 256 * 1024
            || input.images.len() > 4
            || input.images.iter().any(|i| {
                !["image/jpeg", "image/png", "image/webp", "image/gif"]
                    .contains(&i.media_type.as_str())
                    || i.base64.len() > 28 * 1024 * 1024
            })
            || input
                .output_schema
                .as_ref()
                .is_some_and(|s| !s.is_object() || s.to_string().len() > 256 * 1024)
            || input.tools.len() > 128
        {
            return Err(invalid("invalid Agent input"));
        }
        let hash = digest(input)?;
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
            if let Some(record) = ledger.runs.values().find(|r| {
                r.owner.app_id == owner.app_id
                    && r.owner.release_digest == owner.release_digest
                    && r.task.request_id == input.request_id
            }) {
                if record.input_digest != hash {
                    return Err(invalid("requestId conflicts with an earlier request"));
                }
                if record.owner != *owner && !record.task.state.terminal() {
                    return Err(invalid("requestId belongs to an earlier invocation"));
                }
                return Ok(Admission {
                    task: record.task.clone(),
                    start: false,
                    tools,
                });
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
                    task: task.clone(),
                },
            );
            Ok(Admission {
                task,
                start: true,
                tools,
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
                r.owner.app_id == owner.app_id
                    && r.owner.release_digest == owner.release_digest
                    && (r.owner.invocation_id == owner.invocation_id || r.task.state.terminal())
            })
            .ok_or_else(|| AppError::NotFound("Agent task".into()))?;
        Ok(record.task.clone())
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
                .filter(|r| r.owner == *owner)
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
    /// Host executor only; terminal states are immutable. Cancellation is only
    /// finalized after the foreground/tool executor has stopped and reaped.
    pub fn update(
        &self,
        owner: &Owner,
        id: &str,
        state: State,
        result: Option<Value>,
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
}

#[cfg(test)]
mod tests;
