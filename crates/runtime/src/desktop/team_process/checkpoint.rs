//! Q01 in-flight task checkpoint. There is no second persistence format: the
//! canonical Bot×Chat journal (cursors, admitted batches) and the canonical
//! session transcript (prompt, assistant tool calls, tool results, dispatch
//! and result receipts) are the only authorities. This module derives a
//! digest-bound view of them and decides whether a new worker process may
//! continue the task. Per-epoch start tombstones are host-side attempt marks
//! and never carry task progress.
use super::TeamProcessError;
use crate::desktop::session_state::team_chat::{BotChatCursor, BotChatScope};
use lingxi_llm_client::protocol::ProtocolFamily;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(super) const LIST_READ_TOOL: &str = "TeamListRead";
/// A write the ledger makes safe to repeat: the list service applies one operation id once.
pub(super) const LIST_WRITE_TOOL: &str = "TeamListWrite";
/// The same write on a list whose changes the requester approves first: its unfinished call is a request for that approval.
pub(super) const LIST_PROPOSE_TOOL: &str = "TeamListProposeChange";
pub(super) const READ_ONLY_TOOLS: [&str; 3] = ["TeamChatHistory", "TeamKnowledge", LIST_READ_TOOL];
/// The one tool whose unfinished call is a question to the task's requester.
pub(super) const ASK_TOOL: &str = "TeamAskRequester";
pub(super) const MAX_QUESTION_BYTES: usize = 2000;

/// The question of a well-formed ask call. The tool and the checkpoint share
/// this rule, so a call the tool refuses is never reported as a wait.
pub(super) fn ask_question(input: &serde_json::Value) -> Option<&str> {
    let question = input.get("question")?.as_str()?;
    (!question.trim().is_empty() && question.len() <= MAX_QUESTION_BYTES).then_some(question)
}
const TOMBSTONE_PREFIX: &str = "process-started-";
const TOMBSTONE_MAGIC: &str = "team-worker-start-v2";

/// Continuation is enabled only for routes covered by a continuation test: the
/// OpenAI Chat Completions protocol (DeepSeek and other OpenAI-chat-compatible
/// providers). Every other protocol must be re-proved before it is added here.
#[must_use]
pub fn route_supports_continuation(protocol: ProtocolFamily) -> bool {
    matches!(protocol, ProtocolFamily::OpenAiChat)
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamTaskPhase {
    Absent,
    Admitted,
    InFlight,
    ResultRecorded,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeamCheckpointCursor {
    pub session_revision: u64,
    pub last_message_seq: u64,
    pub last_change_seq: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeamPendingToolCall {
    pub logical_step_id: String,
    pub tool_name: String,
    pub read_only: bool,
}

/// What the requester is asked for: an answer in their own words, or a yes or
/// no to one exact operation.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamWaitKind {
    HumanInput,
    ToolApproval,
}

/// What an in-flight task is blocked on: its first unfinished, well-formed
/// ask call or list-change proposal. `wait_id` is derived from the logical
/// step, so every process and the host name the same wait. For an approval,
/// `question` is the proposed write as compact JSON and `request_digest`
/// covers it, so an approval is of exactly these parameters.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeamTaskWait {
    pub wait_id: String,
    pub kind: TeamWaitKind,
    pub logical_step_id: String,
    pub question: String,
    pub request_digest: String,
}

/// Read-only result of `inspect_task`. `checkpoint_digest` covers the scope,
/// task, batch, session revision, both cursors and the identity of the
/// transcript head, so any journal or transcript append changes it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeamTaskCheckpoint {
    pub version: u32,
    pub task_id: String,
    pub phase: TeamTaskPhase,
    pub cursor: TeamCheckpointCursor,
    pub attempts: u32,
    pub dispatches_observed: u32,
    pub pending_tool_calls: Vec<TeamPendingToolCall>,
    pub continuable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// Present when continuing needs the requester's answer to this question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<TeamTaskWait>,
    pub checkpoint_digest: String,
}

/// One unfinished tool call as the worker needs to re-execute it.
#[derive(Debug, Clone)]
pub(super) struct PendingCall {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input_json: String,
    /// Durable per-block assistant row that holds the call.
    pub assistant_row_uuid: String,
    pub logical_step_id: String,
}

/// Everything the transcript says about one task, plus the checkpoint view.
pub(super) struct TaskState {
    pub checkpoint: TeamTaskCheckpoint,
    pub pending: Vec<PendingCall>,
    pub prompt_persisted: bool,
    /// The newest durable assistant response is a complete final answer.
    pub final_answer: Option<String>,
}

pub(super) struct Inputs<'a> {
    pub scope: &'a BotChatScope,
    pub task_id: &'a str,
    pub batch_id: Option<&'a str>,
    pub batch_admitted: bool,
    pub cursor: &'a BotChatCursor,
    pub transcript: &'a str,
    pub attempts: u32,
    pub protocol: ProtocolFamily,
}

/// The logical step of the unfinished call `tool_use_id` of a task, as every
/// later inspection of the same transcript will name it; `None` until the
/// response that holds the call is durable.
pub(super) fn pending_step(
    scope: &BotChatScope,
    task_id: &str,
    batch_id: &str,
    transcript: &str,
    tool_use_id: &str,
) -> Option<String> {
    let state = evaluate(&Inputs {
        scope,
        task_id,
        batch_id: Some(batch_id),
        batch_admitted: true,
        cursor: &BotChatCursor::default(),
        transcript,
        attempts: 0,
        protocol: ProtocolFamily::AnthropicMessages,
    })
    .ok()?;
    state
        .pending
        .into_iter()
        .find(|call| call.tool_use_id == tool_use_id)
        .map(|call| call.logical_step_id)
}

pub(super) fn task_dir(home: &Path, task_id: &str) -> PathBuf {
    home.join("team-tasks")
        .join(format!("{:x}", Sha256::digest(task_id.as_bytes())))
}

pub(super) fn prompt_uuid(task_id: &str) -> uuid::Uuid {
    super::chat_session::uuid(&format!("team-task-prompt:{task_id}"))
}

fn tombstone_body(task_id: &str, batch_id: &str, epoch: u32) -> Result<Vec<u8>, TeamProcessError> {
    let mut body = TOMBSTONE_MAGIC.as_bytes().to_vec();
    body.push(b'\n');
    body.extend(
        serde_json::to_vec(&serde_json::json!({"taskId":task_id,"batchId":batch_id,"epoch":epoch}))
            .map_err(|_| TeamProcessError::Protocol)?,
    );
    body.push(b'\n');
    Ok(body)
}

/// Create-new per-epoch start tombstone, durable before any worker is spawned.
/// Epoch 1 is the first `run`; a continuation epoch must exceed every recorded
/// epoch. A repeated or stale epoch is `AlreadyAttempted`.
pub(super) fn record_start(
    home: &Path,
    task_id: &str,
    batch_id: &str,
    epoch: u32,
) -> Result<(), TeamProcessError> {
    let dir = task_dir(home, task_id);
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&dir)?;
    }
    let recorded = epochs(&dir)?;
    if recorded.iter().any(|(recorded, _)| *recorded >= epoch) {
        return Err(TeamProcessError::AlreadyAttempted);
    }
    if epoch != 1 && recorded.is_empty() {
        return Err(TeamProcessError::NotContinuable);
    }
    if let Some((_, first)) = recorded.first() {
        if first.batch_id != batch_id {
            return Err(TeamProcessError::Protocol);
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut file = match options.open(dir.join(format!("{TOMBSTONE_PREFIX}{epoch}"))) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(TeamProcessError::AlreadyAttempted);
        }
        Err(e) => return Err(e.into()),
    };
    std::io::Write::write_all(&mut file, &tombstone_body(task_id, batch_id, epoch)?)?;
    file.sync_all()?;
    std::fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

pub(super) struct Tombstone {
    pub batch_id: String,
}

/// Recorded epochs in ascending order. A tombstone that is unreadable or
/// disagrees with its own file name is storage corruption, never ignored.
pub(super) fn epochs(dir: &Path) -> Result<Vec<(u32, Tombstone)>, TeamProcessError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(TeamProcessError::Protocol)?;
        let epoch: u32 = name
            .strip_prefix(TOMBSTONE_PREFIX)
            .and_then(|n| n.parse().ok())
            .filter(|n| *n >= 1)
            .ok_or(TeamProcessError::Protocol)?;
        let text = std::fs::read_to_string(entry.path())?;
        let mut lines = text.lines();
        let value: serde_json::Value = (lines.next() == Some(TOMBSTONE_MAGIC))
            .then(|| lines.next())
            .flatten()
            .and_then(|line| serde_json::from_str(line).ok())
            .ok_or(TeamProcessError::Protocol)?;
        if value["epoch"].as_u64() != Some(u64::from(epoch)) {
            return Err(TeamProcessError::Protocol);
        }
        let batch_id = value["batchId"]
            .as_str()
            .ok_or(TeamProcessError::Protocol)?
            .to_owned();
        out.push((epoch, Tombstone { batch_id }));
    }
    out.sort_by_key(|(epoch, _)| *epoch);
    Ok(out)
}

struct Call {
    id: String,
    name: String,
    input_json: String,
    row_uuid: String,
    group: usize,
    index: usize,
}

#[derive(Default)]
struct Group {
    calls: usize,
    stop_reason: Option<String>,
    text: String,
}

/// Fold one task's durable transcript rows. Rows of the task are those after
/// the last projected row of its admitted batch; every row a Team worker
/// writes for the task is append-only and ordered by the single writer.
pub(super) fn evaluate(input: &Inputs<'_>) -> Result<TaskState, TeamProcessError> {
    let prompt = prompt_uuid(input.task_id).to_string();
    let mut rows = 0usize;
    let mut result_recorded = false;
    let mut dispatches = 0u32;
    let mut in_segment = false;
    let mut prompt_persisted = false;
    let mut calls: Vec<Call> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut group_key: Option<String> = None;
    let mut last_kind: Option<&str> = None;
    let mut inconsistent = false;
    for line in input.transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows += 1;
        let row: serde_json::Value =
            serde_json::from_str(line).map_err(|_| TeamProcessError::Protocol)?;
        let task_matches = row["taskId"] == input.task_id;
        match (row["type"].as_str(), row["subtype"].as_str()) {
            (Some("system"), Some("team_bot_result")) if task_matches => {
                result_recorded = true;
                continue;
            }
            (Some("system"), Some("team_bot_dispatch")) if task_matches => {
                dispatches = dispatches.saturating_add(1);
                continue;
            }
            _ => {}
        }
        if row["origin"]["kind"] == "shared_chat" {
            if Some(row["origin"]["batchId"].as_str().unwrap_or_default()) == input.batch_id {
                in_segment = true;
                prompt_persisted = false;
                calls.clear();
                results.clear();
                groups.clear();
                group_key = None;
                last_kind = None;
                inconsistent = false;
            } else if in_segment {
                // Another batch was admitted after this task's input.
                inconsistent = true;
            }
            continue;
        }
        if !in_segment || result_recorded {
            continue;
        }
        let meta = row["isMeta"].as_bool().unwrap_or(false);
        match row["type"].as_str() {
            Some("user") if !meta => {
                let content = &row["message"]["content"];
                let tool_results = content
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "tool_result")
                            .filter_map(|b| b["tool_use_id"].as_str())
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !tool_results.is_empty() {
                    results.extend(tool_results);
                    last_kind = Some("tool_result");
                } else if row["uuid"] == prompt.as_str() {
                    prompt_persisted = true;
                    last_kind = Some("prompt");
                } else {
                    inconsistent = true;
                }
            }
            Some("assistant") => {
                if row["isApiErrorMessage"].as_bool().unwrap_or(false) {
                    inconsistent = true;
                    continue;
                }
                let key = row["message"]["id"]
                    .as_str()
                    .or_else(|| row["uuid"].as_str())
                    .ok_or(TeamProcessError::Protocol)?
                    .to_owned();
                if group_key.as_deref() != Some(key.as_str()) {
                    group_key = Some(key);
                    groups.push(Group::default());
                }
                let group = groups.len() - 1;
                let current = groups.last_mut().expect("group was just ensured");
                if let Some(reason) = row["message"]["stop_reason"].as_str() {
                    current.stop_reason = Some(reason.to_owned());
                }
                for block in row["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("tool_use") => {
                            let (Some(id), Some(name)) =
                                (block["id"].as_str(), block["name"].as_str())
                            else {
                                inconsistent = true;
                                continue;
                            };
                            calls.push(Call {
                                id: id.to_owned(),
                                name: name.to_owned(),
                                input_json: serde_json::to_string(&block["input"])
                                    .map_err(|_| TeamProcessError::Protocol)?,
                                row_uuid: row["uuid"].as_str().unwrap_or_default().to_owned(),
                                group,
                                index: current.calls,
                            });
                            current.calls += 1;
                        }
                        Some("text") => {
                            current
                                .text
                                .push_str(block["text"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                last_kind = Some("assistant");
            }
            _ => {}
        }
    }
    let last_group = groups.len().checked_sub(1);
    // Every result must answer exactly one known call, and only the newest
    // response may still have unanswered calls.
    let mut answered = std::collections::HashSet::new();
    for id in &results {
        if !calls.iter().any(|c| &c.id == id) || !answered.insert(id.as_str()) {
            inconsistent = true;
        }
    }
    let pending_calls: Vec<&Call> = calls
        .iter()
        .filter(|c| !answered.contains(c.id.as_str()))
        .collect();
    if pending_calls.iter().any(|c| Some(c.group) != last_group) {
        inconsistent = true;
    }
    let mut final_answer = None;
    if let Some(group) = last_group.map(|g| &groups[g]) {
        let reason = group.stop_reason.as_deref();
        if group.calls == 0 {
            if matches!(reason, Some("end_turn" | "stop_sequence")) && !group.text.trim().is_empty()
            {
                final_answer = Some(group.text.clone());
            } else {
                // Cut-off text or a tool-use response whose call rows were
                // never written cannot be completed without guessing.
                inconsistent = true;
            }
        }
        if last_kind == Some("prompt") {
            inconsistent = true;
        }
    }
    let phase = if result_recorded {
        TeamTaskPhase::ResultRecorded
    } else if !input.batch_admitted {
        TeamTaskPhase::Absent
    } else if dispatches > 0 || prompt_persisted || !groups.is_empty() || !results.is_empty() {
        TeamTaskPhase::InFlight
    } else {
        TeamTaskPhase::Admitted
    };
    let pending_tool_calls: Vec<TeamPendingToolCall> = if phase == TeamTaskPhase::InFlight {
        pending_calls
            .iter()
            .map(|c| TeamPendingToolCall {
                logical_step_id: format!("{}/t{}/c{}", input.task_id, c.group, c.index),
                tool_name: c.name.clone(),
                read_only: READ_ONLY_TOOLS.contains(&c.name.as_str()),
            })
            .collect()
    } else {
        Vec::new()
    };
    let (continuable, reason) = if !route_supports_continuation(input.protocol) {
        (false, Some("route_unsupported"))
    } else {
        match phase {
            TeamTaskPhase::ResultRecorded => (true, None),
            TeamTaskPhase::Absent => (false, Some("not_admitted")),
            _ if inconsistent => (false, Some("transcript_inconsistent")),
            _ if pending_tool_calls
                .iter()
                .any(|c| {
                    !c.read_only
                        && ![ASK_TOOL, LIST_WRITE_TOOL, LIST_PROPOSE_TOOL]
                            .contains(&c.tool_name.as_str())
                }) =>
            {
                (false, Some("pending_write_tool"))
            }
            _ => (true, None),
        }
    };
    let cursor = TeamCheckpointCursor {
        session_revision: input.cursor.session_revision,
        last_message_seq: input.cursor.last_message_seq,
        last_change_seq: input.cursor.last_change_seq,
    };
    let digest = digest(input, &cursor, rows)?;
    let wait = if phase == TeamTaskPhase::InFlight && continuable {
        pending_calls.iter().find_map(|c| {
            let asked: serde_json::Value = serde_json::from_str(&c.input_json).ok()?;
            let (kind, question) = match c.name.as_str() {
                ASK_TOOL => (TeamWaitKind::HumanInput, ask_question(&asked)?.to_owned()),
                LIST_PROPOSE_TOOL => {
                    super::list_tools::validate_write(&asked).ok()?;
                    (TeamWaitKind::ToolApproval, asked.to_string())
                }
                _ => return None,
            };
            let logical_step_id = format!("{}/t{}/c{}", input.task_id, c.group, c.index);
            Some(wait_of(input.task_id, logical_step_id, kind, question))
        })
    } else {
        None
    };
    let pending = if phase == TeamTaskPhase::InFlight {
        pending_calls
            .iter()
            .map(|c| PendingCall {
                tool_use_id: c.id.clone(),
                tool_name: c.name.clone(),
                input_json: c.input_json.clone(),
                assistant_row_uuid: c.row_uuid.clone(),
                logical_step_id: format!("{}/t{}/c{}", input.task_id, c.group, c.index),
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(TaskState {
        checkpoint: TeamTaskCheckpoint {
            version: 3,
            task_id: input.task_id.to_owned(),
            phase,
            cursor,
            attempts: input.attempts,
            dispatches_observed: dispatches,
            pending_tool_calls,
            continuable,
            reason,
            wait,
            checkpoint_digest: digest,
        },
        pending,
        prompt_persisted,
        final_answer: final_answer.filter(|_| pending_calls.is_empty()),
    })
}

/// The wait a tool call at `logical_step_id` opens, whichever process names it.
pub(super) fn wait_id(logical_step_id: &str) -> String {
    format!(
        "wait_{}",
        &format!(
            "{:x}",
            Sha256::digest(format!("team-wait:{logical_step_id}").as_bytes())
        )[..40]
    )
}

fn wait_of(
    task_id: &str,
    logical_step_id: String,
    kind: TeamWaitKind,
    question: String,
) -> TeamTaskWait {
    let wait_id = wait_id(&logical_step_id);
    let request = match kind {
        TeamWaitKind::HumanInput => {
            serde_json::json!({"domain":"lingxi-team-wait-1","taskId":task_id,"waitId":wait_id,"question":question})
        }
        TeamWaitKind::ToolApproval => {
            serde_json::json!({"domain":"lingxi-team-approval-1","taskId":task_id,"waitId":wait_id,"tool":LIST_WRITE_TOOL,"arguments":question})
        }
    };
    let request_digest = format!("{:x}", Sha256::digest(request.to_string().as_bytes()));
    TeamTaskWait {
        wait_id,
        kind,
        logical_step_id,
        question,
        request_digest,
    }
}

fn digest(
    input: &Inputs<'_>,
    cursor: &TeamCheckpointCursor,
    rows: usize,
) -> Result<String, TeamProcessError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Covered<'a> {
        domain: &'static str,
        scope: &'a BotChatScope,
        task_id: &'a str,
        batch_id: Option<&'a str>,
        session_revision: u64,
        last_message_seq: u64,
        last_change_seq: u64,
        bootstrap_from_seq: u64,
        transcript_rows: usize,
        transcript_bytes: usize,
        transcript_sha256: String,
    }
    let bytes = serde_json::to_vec(&Covered {
        domain: "lingxi-team-task-checkpoint-3",
        scope: input.scope,
        task_id: input.task_id,
        batch_id: input.batch_id,
        session_revision: cursor.session_revision,
        last_message_seq: cursor.last_message_seq,
        last_change_seq: cursor.last_change_seq,
        bootstrap_from_seq: input.cursor.bootstrap_from_seq,
        transcript_rows: rows,
        transcript_bytes: input.transcript.len(),
        transcript_sha256: format!("{:x}", Sha256::digest(input.transcript.as_bytes())),
    })
    .map_err(|_| TeamProcessError::Protocol)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TASK: &str = "task_unit_0001";
    const BATCH: &str = "batch_unit_0001";

    fn scope() -> BotChatScope {
        BotChatScope {
            team_id: "team_0001".into(),
            shared_bot_id: "sharedbot_01".into(),
            conversation_id: "chat_0001".into(),
            session_epoch: 1,
        }
    }
    fn lines(rows: Vec<serde_json::Value>) -> String {
        rows.iter().map(|r| format!("{r}\n")).collect()
    }
    fn batch_row() -> serde_json::Value {
        json!({"type":"user","uuid":"row_batch_1","origin":{"kind":"shared_chat","batchId":BATCH},"message":{"role":"user","content":"SOURCE"}})
    }
    fn prompt_row() -> serde_json::Value {
        json!({"type":"user","uuid":prompt_uuid(TASK).to_string(),"message":{"role":"user","content":"prompt"}})
    }
    fn call_row(group: &str, calls: &[(&str, &str)], stop: &str) -> serde_json::Value {
        json!({"type":"assistant","uuid":format!("row_{group}_{}", calls.first().map_or("x", |c| c.0)),
            "message":{"id":group,"stop_reason":stop,"content":calls.iter().map(|(id,name)|json!({"type":"tool_use","id":id,"name":name,"input":{"kind":"before"}})).collect::<Vec<_>>()}})
    }
    fn result_row(id: &str) -> serde_json::Value {
        json!({"type":"user","uuid":format!("row_result_{id}"),"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"ok"}]}})
    }
    fn text_row(group: &str, text: &str, stop: &str) -> serde_json::Value {
        json!({"type":"assistant","uuid":format!("row_text_{group}"),"message":{"id":group,"stop_reason":stop,"content":[{"type":"text","text":text}]}})
    }
    fn dispatch_row(n: u32) -> serde_json::Value {
        json!({"type":"system","subtype":"team_bot_dispatch","uuid":format!("d{n}"),"taskId":TASK,"epoch":1,"sequence":n})
    }
    fn eval(transcript: &str, admitted: bool, protocol: ProtocolFamily) -> TaskState {
        evaluate(&Inputs {
            scope: &scope(),
            task_id: TASK,
            batch_id: Some(BATCH),
            batch_admitted: admitted,
            cursor: &BotChatCursor {
                scope: Some(scope()),
                session_revision: 1,
                last_message_seq: 2,
                last_change_seq: 2,
                bootstrap_from_seq: 0,
            },
            transcript,
            attempts: 1,
            protocol,
        })
        .unwrap()
    }
    const CHAT: ProtocolFamily = ProtocolFamily::OpenAiChat;

    #[test]
    fn unfinished_read_only_call_is_pending_with_a_provider_independent_step_id() {
        let t = lines(vec![
            batch_row(),
            dispatch_row(1),
            prompt_row(),
            call_row(
                "m1",
                &[
                    ("provider_call_a", "TeamChatHistory"),
                    ("provider_call_b", "TeamKnowledge"),
                ],
                "tool_use",
            ),
            result_row("provider_call_a"),
        ]);
        let state = eval(&t, true, CHAT);
        let c = &state.checkpoint;
        assert_eq!(c.phase, TeamTaskPhase::InFlight);
        assert!(c.continuable, "{:?}", c.reason);
        assert_eq!(c.dispatches_observed, 1);
        assert_eq!(c.pending_tool_calls.len(), 1);
        assert_eq!(
            c.pending_tool_calls[0].logical_step_id,
            format!("{TASK}/t0/c1")
        );
        assert!(!c.pending_tool_calls[0]
            .logical_step_id
            .contains("provider_call"));
        assert_eq!(state.pending[0].tool_use_id, "provider_call_b");
        assert!(state.prompt_persisted);
    }
    #[test]
    fn pending_write_tool_is_never_continuable() {
        let t = lines(vec![
            batch_row(),
            prompt_row(),
            call_row("m1", &[("c1", "Bash")], "tool_use"),
        ]);
        let c = eval(&t, true, CHAT).checkpoint;
        assert!(!c.continuable);
        assert_eq!(c.reason, Some("pending_write_tool"));
        assert!(!c.pending_tool_calls[0].read_only);
    }
    #[test]
    fn inconsistent_transcripts_are_refused_not_guessed() {
        for rows in [
            // result with no call
            vec![batch_row(), prompt_row(), result_row("ghost")],
            // older response still has an unanswered call
            vec![
                batch_row(),
                prompt_row(),
                call_row("m1", &[("c1", "TeamChatHistory")], "tool_use"),
                call_row("m2", &[("c2", "TeamChatHistory")], "tool_use"),
            ],
            // tool-use response whose call rows were never written
            vec![
                batch_row(),
                prompt_row(),
                text_row("m1", "thinking", "tool_use"),
            ],
            // cut-off answer
            vec![
                batch_row(),
                prompt_row(),
                text_row("m1", "half", "max_tokens"),
            ],
            // duplicate result
            vec![
                batch_row(),
                prompt_row(),
                call_row("m1", &[("c1", "TeamChatHistory")], "tool_use"),
                result_row("c1"),
                result_row("c1"),
            ],
        ] {
            let c = eval(&lines(rows), true, CHAT).checkpoint;
            assert!(!c.continuable);
            assert_eq!(c.reason, Some("transcript_inconsistent"));
        }
    }
    #[test]
    fn final_answer_without_result_row_finishes_without_a_model_call() {
        let t = lines(vec![
            batch_row(),
            prompt_row(),
            text_row("m1", "All done", "end_turn"),
        ]);
        let state = eval(&t, true, CHAT);
        assert!(state.checkpoint.continuable);
        assert_eq!(state.final_answer.as_deref(), Some("All done"));
        assert!(state.pending.is_empty());
    }
    #[test]
    fn phases_route_and_digest() {
        let admitted = lines(vec![batch_row()]);
        assert_eq!(
            eval(&admitted, true, CHAT).checkpoint.phase,
            TeamTaskPhase::Admitted
        );
        assert!(eval(&admitted, true, CHAT).checkpoint.continuable);
        let absent = eval(&admitted, false, CHAT).checkpoint;
        assert_eq!(absent.phase, TeamTaskPhase::Absent);
        assert_eq!(absent.reason, Some("not_admitted"));
        let recorded = lines(vec![
            batch_row(),
            prompt_row(),
            json!({"type":"system","subtype":"team_bot_result","uuid":"res","taskId":TASK}),
        ]);
        let done = eval(&recorded, true, CHAT).checkpoint;
        assert_eq!(done.phase, TeamTaskPhase::ResultRecorded);
        assert!(done.continuable);
        // Route support is decided first and never changes the digest.
        let flight = lines(vec![batch_row(), prompt_row()]);
        let supported = eval(&flight, true, CHAT).checkpoint;
        let other = eval(&flight, true, ProtocolFamily::AnthropicMessages).checkpoint;
        assert!(supported.continuable && !other.continuable);
        assert_eq!(other.reason, Some("route_unsupported"));
        assert_eq!(supported.checkpoint_digest, other.checkpoint_digest);
        assert!(valid_digest(&supported.checkpoint_digest));
        // Any append moves the digest.
        let longer = format!("{flight}{}\n", dispatch_row(1));
        assert_ne!(
            eval(&longer, true, CHAT).checkpoint.checkpoint_digest,
            supported.checkpoint_digest
        );
        // Another task's receipts are not ours.
        let foreign = json!({"type":"system","subtype":"team_bot_dispatch","uuid":"x","taskId":"someone_else"});
        let c = eval(&format!("{flight}{foreign}\n"), true, CHAT).checkpoint;
        assert_eq!(c.dispatches_observed, 0);
    }
    #[test]
    fn epoch_tombstones_are_create_new_and_strictly_increasing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            record_start(&home, TASK, BATCH, 2),
            Err(TeamProcessError::NotContinuable)
        ));
        record_start(&home, TASK, BATCH, 1).unwrap();
        assert!(matches!(
            record_start(&home, TASK, BATCH, 1),
            Err(TeamProcessError::AlreadyAttempted)
        ));
        record_start(&home, TASK, BATCH, 3).unwrap();
        assert!(matches!(
            record_start(&home, TASK, BATCH, 2),
            Err(TeamProcessError::AlreadyAttempted)
        ));
        assert!(matches!(
            record_start(&home, TASK, "another_batch_id", 4),
            Err(TeamProcessError::Protocol)
        ));
        let recorded = epochs(&task_dir(&home, TASK)).unwrap();
        assert_eq!(
            recorded.iter().map(|(e, _)| *e).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(recorded[0].1.batch_id, BATCH);
    }
}
