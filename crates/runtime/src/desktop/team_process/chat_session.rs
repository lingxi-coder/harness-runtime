//! Bot×Chat uses the existing session journal, captured resume and transcript writer.
use super::TeamProcessError;
use crate::desktop::session_state::{
    team_chat::{BotChatCursor, BotChatScope, ChatAuthor, ChatInputBatch, ChatSourceMessage},
    SessionStateCoordinator,
};
use crate::desktop::{team_invocation::TeamInvocationProfile, DesktopRuntime};
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::{ConversationMessage, MessageId};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};
use tokio_util::sync::CancellationToken;
fn storage_error<T>(_: T) -> TeamProcessError {
    TeamProcessError::Worker
}
pub fn inspect_scope(
    storage_root: &Path,
    scope: &BotChatScope,
) -> Result<BotChatCursor, TeamProcessError> {
    let id = scope.session_id().map_err(storage_error)?;
    let home = storage_root
        .join("scopes")
        .join(id.as_uuid().to_string())
        .join("home");
    if !home.exists() {
        return Ok(BotChatCursor::default());
    }
    if home.canonicalize()? != home {
        return Err(TeamProcessError::Protocol);
    }
    let lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
        .claim_session_id(&id.to_string(), std::process::id())
        .map_err(storage_error)?
        .into_shared();
    let state = SessionStateCoordinator::open(home, id, lease).map_err(storage_error)?;
    state.hydrate_blocking().map_err(storage_error)?;
    Ok(state.bot_chat_cursor())
}
/// Read-only Q01 view of one task. Taken under the session writer lease; it
/// never mounts the runtime, never appends and never calls a model.
pub fn inspect_task(
    storage_root: &Path,
    scope: &BotChatScope,
    task_id: &str,
    protocol: lingxi_llm_client::protocol::ProtocolFamily,
) -> Result<super::TeamTaskCheckpoint, TeamProcessError> {
    if !super::protocol::valid_id(task_id) {
        return Err(TeamProcessError::Protocol);
    }
    let id = scope.session_id().map_err(storage_error)?;
    let scope_root = storage_root.join("scopes").join(id.as_uuid().to_string());
    let home = scope_root.join("home");
    let epochs = super::checkpoint::epochs(&super::checkpoint::task_dir(&home, task_id))?;
    let batch_id = epochs.first().map(|(_, t)| t.batch_id.clone());
    let attempts = u32::try_from(epochs.len()).map_err(storage_error)?;
    let (cursor, admitted, transcript) = if home.exists() {
        if home.canonicalize()? != home {
            return Err(TeamProcessError::Protocol);
        }
        let lease =
            lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
                .claim_session_id(&id.to_string(), std::process::id())
                .map_err(storage_error)?
                .into_shared();
        let state = SessionStateCoordinator::open(home.clone(), id, lease).map_err(storage_error)?;
        state.hydrate_blocking().map_err(storage_error)?;
        let admitted = match &batch_id {
            Some(wanted) => state
                .chat_input_batches()
                .map_err(storage_error)?
                .iter()
                .any(|batch| &batch.batch_id == wanted),
            None => false,
        };
        let path = session::jsonl::session_path(
            &home,
            &scope_root.join("workspace").to_string_lossy(),
            &id.as_uuid().to_string(),
        );
        let transcript = if path.exists() {
            let relative = path.strip_prefix(&home).map_err(storage_error)?.to_owned();
            let (_, bytes) =
                session::jsonl::writer::TranscriptFileAuthority::capture(home.clone(), relative)
                    .map_err(storage_error)?;
            String::from_utf8(bytes).map_err(storage_error)?
        } else {
            String::new()
        };
        (state.bot_chat_cursor(), admitted, transcript)
    } else {
        (BotChatCursor::default(), false, String::new())
    };
    Ok(super::checkpoint::evaluate(&super::checkpoint::Inputs {
        scope,
        task_id,
        batch_id: batch_id.as_deref(),
        batch_admitted: admitted,
        cursor: &cursor,
        transcript: &transcript,
        attempts,
        protocol,
    })?
    .checkpoint)
}

/// Durable receipt written under the writer lease immediately before each
/// physical provider dispatch is allowed. It over-counts (never under-counts)
/// requests whose response was lost with the process.
pub(super) struct DispatchLedger {
    task_id: String,
    epoch: u32,
    session_id: lingxi_core::types::SessionId,
    next: std::sync::atomic::AtomicU32,
    writer: std::sync::OnceLock<std::sync::Arc<session::jsonl::JsonlWriter>>,
}
impl DispatchLedger {
    pub(super) fn new(
        task_id: String,
        epoch: u32,
        session_id: lingxi_core::types::SessionId,
    ) -> Self {
        Self {
            task_id,
            epoch,
            session_id,
            next: std::sync::atomic::AtomicU32::new(1),
            writer: std::sync::OnceLock::new(),
        }
    }
    pub(super) fn bind(&self, rt: &DesktopRuntime) -> Result<(), TeamProcessError> {
        let writer = rt
            .orchestrator
            .session_transcript_writer()
            .ok_or(TeamProcessError::Worker)?;
        self.writer.set(writer).map_err(|_| TeamProcessError::Worker)
    }
    pub(super) async fn record(&self) -> Result<(), TeamProcessError> {
        let writer = self.writer.get().ok_or(TeamProcessError::Worker)?;
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let event = format!("team-dispatch:{}:{}:{n}", self.task_id, self.epoch);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let row = serde_json::json!({"type":"system","subtype":"team_bot_dispatch","uuid":uuid(&event),"sessionId":self.session_id.as_uuid().to_string(),"parentUuid":null,"timestamp":now,"isSidechain":false,"isMeta":true,"isVisibleInTranscriptOnly":true,"message":{"role":"system","content":""},"taskId":self.task_id,"epoch":self.epoch,"sequence":n});
        writer
            .append_json_once_durable(
                &event,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&row)
                    .map_err(storage_error)?,
            )
            .await
            .map_err(storage_error)?;
        Ok(())
    }
}

pub(super) fn uuid(key: &str) -> uuid::Uuid {
    let h = Sha256::digest(key.as_bytes());
    let mut b: [u8; 16] = h[..16].try_into().unwrap();
    b[6] = (b[6] & 15) | 128;
    b[8] = (b[8] & 63) | 128;
    uuid::Uuid::from_bytes(b)
}
fn text(source: &ChatSourceMessage) -> Result<String, TeamProcessError> {
    serde_json::to_string(source)
        .map(|json| format!("<shared_chat_message>\n{json}\n</shared_chat_message>"))
        .map_err(|_| TeamProcessError::Protocol)
}
fn message(
    batch: &ChatInputBatch,
    source: &ChatSourceMessage,
) -> Result<ConversationMessage, TeamProcessError> {
    Ok(ConversationMessage::user(
        MessageId::from_uuid(uuid(&format!(
            "{}:{}:{}",
            batch.batch_id, source.message_id, source.change_id
        ))),
        text(source)?,
    ))
}
fn source_map(batches: &[ChatInputBatch]) -> BTreeMap<u64, ChatSourceMessage> {
    let mut values = BTreeMap::new();
    for batch in batches {
        for message in batch.messages.iter().chain(&batch.changes) {
            values.insert(message.seq, message.clone());
        }
    }
    values
}
fn own_echo(source: &ChatSourceMessage, scope: &BotChatScope, tasks: &HashSet<String>) -> bool {
    matches!(&source.author,ChatAuthor::SharedBot{shared_bot_id,..}if shared_bot_id==&scope.shared_bot_id)
        && source
            .source_task_id
            .as_ref()
            .is_some_and(|task| tasks.contains(task))
}
async fn project_batch(
    rt: &DesktopRuntime,
    batch: &ChatInputBatch,
    prior: &[ChatInputBatch],
    projected: &mut HashMap<String, serde_json::Value>,
    echo_tasks: &HashSet<String>,
    last_unconfirmed: &mut Option<(session::JsonlMessage, bool)>,
) -> Result<(), TeamProcessError> {
    let session_id = batch
        .scope()
        .session_id()
        .map_err(storage_error)?
        .as_uuid()
        .to_string();
    let cwd = rt
        .orchestrator
        .session_transcript_writer()
        .and_then(|w| w.session_target_cwd(batch.scope().session_id().ok()?))
        .ok_or(TeamProcessError::Worker)?
        .to_string_lossy()
        .into_owned();
    let timestamp = chrono::DateTime::from_timestamp_millis(
        batch
            .messages
            .last()
            .ok_or(TeamProcessError::Protocol)?
            .created_at as i64,
    )
    .ok_or(TeamProcessError::Protocol)?
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let base = |id: uuid::Uuid| serde_json::json!({"type":"user","uuid":id.to_string(),"parentUuid":null,"sessionId":session_id,"timestamp":timestamp,"cwd":cwd,"version":env!("CARGO_PKG_VERSION"),"isSidechain":false,"userType":"external","message":{"role":"user","content":[]}});
    let sources = if batch.changes.is_empty() {
        batch.messages.clone()
    } else {
        let mut row = base(uuid(&format!("team-source-reset:{}", batch.batch_id)));
        row["type"] = "system".into();
        row["subtype"] = "compact_boundary".into();
        row["content"] =
            "Shared Chat sources changed; rebuilt from current original messages.".into();
        row["level"] = "info".into();
        row["compactMetadata"] = serde_json::json!({"trigger":"manual","preTokens":0});
        row["message"] = serde_json::Value::Null;
        let row: session::JsonlMessage = serde_json::from_value(row).map_err(storage_error)?;
        project_row(rt, row, None, true, projected, last_unconfirmed).await?;
        source_map(prior).into_values().collect()
    };
    for source in sources {
        if batch.changes.is_empty() && own_echo(&source, &batch.scope(), echo_tasks) {
            continue;
        }
        let msg = message(batch, &source)?;
        let mut row = base(msg.id().as_uuid());
        row["timestamp"] = chrono::DateTime::from_timestamp_millis(source.created_at as i64)
            .ok_or(TeamProcessError::Protocol)?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            .into();
        row["origin"] =
            serde_json::json!({"kind":"shared_chat","batchId":batch.batch_id,"source":source});
        row["message"] = serde_json::json!({"role":"user","content":text(&source)?});
        let row: session::JsonlMessage = serde_json::from_value(row).map_err(storage_error)?;
        project_row(rt, row, Some(msg), false, projected, last_unconfirmed).await?;
    }
    Ok(())
}
fn immutable_row(mut row: serde_json::Value) -> serde_json::Value {
    if let Some(object) = row.as_object_mut() {
        object.remove("parentUuid");
        object.remove("timestamp");
    }
    row
}
async fn project_row(
    rt: &DesktopRuntime,
    row: session::JsonlMessage,
    message: Option<ConversationMessage>,
    reset: bool,
    index: &mut HashMap<String, serde_json::Value>,
    last_unconfirmed: &mut Option<(session::JsonlMessage, bool)>,
) -> Result<(), TeamProcessError> {
    let expected = immutable_row(serde_json::to_value(&row).map_err(storage_error)?);
    if let Some(actual) = index.get(&row.uuid) {
        if immutable_row(actual.clone()) != expected {
            return Err(TeamProcessError::Protocol);
        }
        *last_unconfirmed = Some((row, reset));
        return Ok(());
    }
    let id = row.uuid.clone();
    rt.orchestrator
        .project_external_source_row(row, message, reset)
        .await
        .map_err(storage_error)?;
    *last_unconfirmed = None; // This durable append fsynced the whole prior prefix.
    index.insert(id, expected);
    Ok(())
}
pub(super) async fn repair_projection(rt: &DesktopRuntime) -> Result<(), TeamProcessError> {
    let batches = rt
        .session_state
        .chat_input_batches()
        .map_err(storage_error)?;
    if batches.is_empty() {
        return Ok(());
    }
    let id = batches[0].scope().session_id().map_err(storage_error)?;
    let capture = rt
        .orchestrator
        .session_transcript_writer()
        .ok_or(TeamProcessError::Worker)?
        .capture_committed_transcript(id.as_uuid())
        .await
        .map_err(storage_error)?;
    let mut projected = HashMap::new();
    let mut echo_tasks = HashSet::new();
    let mut last_unconfirmed = None;
    for line in capture.source.lines() {
        let row: serde_json::Value = serde_json::from_str(line).map_err(storage_error)?;
        if row["type"] == "system" && row["subtype"] == "team_bot_result" {
            if let Some(task) = row["taskId"].as_str() {
                echo_tasks.insert(task.to_owned());
            }
        }
        if row["origin"]["kind"] == "shared_chat"
            || (row["type"] == "system" && row["subtype"] == "compact_boundary")
        {
            if let Some(id) = row["uuid"].as_str() {
                projected.insert(id.to_owned(), row);
            }
        }
    }
    for (index, batch) in batches.iter().enumerate() {
        project_batch(
            rt,
            batch,
            &batches[..=index],
            &mut projected,
            &echo_tasks,
            &mut last_unconfirmed,
        )
        .await?;
    }
    // The capture can contain a row written before an acknowledgement failure.
    // One duplicate append-once sync certifies that entire existing prefix.
    if let Some((row, reset)) = last_unconfirmed {
        rt.orchestrator
            .project_external_source_row(row, None, reset)
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}
/// Static capacity admission includes the configured model window, real current
/// prompt/tool overhead and full current batch; no cursor changes on failure.
pub(super) async fn admit(
    rt: &DesktopRuntime,
    profile: &TeamInvocationProfile,
    batch: ChatInputBatch,
    cancel: CancellationToken,
) -> Result<(), TeamProcessError> {
    batch
        .validate(profile.session_id())
        .map_err(storage_error)?;
    let cursor = rt.session_state.bot_chat_cursor();
    if cursor.session_revision != batch.expected_session_revision {
        return Err(TeamProcessError::Protocol);
    }
    let incoming = if batch.changes.is_empty() {
        batch.messages.clone()
    } else {
        let mut all = rt
            .session_state
            .chat_input_batches()
            .map_err(storage_error)?;
        all.push(batch.clone());
        source_map(&all).into_values().collect()
    };
    let candidate = incoming
        .iter()
        .map(|s| message(&batch, s))
        .collect::<Result<Vec<_>, _>>()?;
    let incoming_tokens = compaction::grouping::estimate_tokens_for_range(&candidate) as u64;
    let snapshot = rt
        .orchestrator
        .context_usage_snapshot()
        .await
        .map_err(storage_error)?;
    let window =
        u64::from(profile.context_window_tokens()).min(if snapshot.max_context_tokens > 0 {
            snapshot.max_context_tokens
        } else {
            u64::from(profile.context_window_tokens())
        });
    let reserve = u64::from(profile.output_quota().max_output_tokens_per_request()) + 2048;
    let current_history = rt.orchestrator.session().lock().await.history.clone();
    let history_tokens = compaction::grouping::estimate_tokens_for_range(&current_history) as u64;
    let fixed = snapshot
        .live_context_tokens
        .saturating_sub(history_tokens)
        .max(2048);
    if incoming_tokens
        .saturating_add(fixed)
        .saturating_add(reserve)
        > window
    {
        return Err(TeamProcessError::Capacity);
    }
    if batch.changes.is_empty()
        && snapshot
            .live_context_tokens
            .saturating_add(incoming_tokens)
            .saturating_add(reserve)
            > window
    {
        rt.orchestrator
            .force_compact_with_cancel(cancel)
            .await
            .map_err(|_| TeamProcessError::Capacity)?;
        let after = rt
            .orchestrator
            .context_usage_snapshot()
            .await
            .map_err(storage_error)?;
        if after
            .live_context_tokens
            .saturating_add(incoming_tokens)
            .saturating_add(reserve)
            > window
        {
            return Err(TeamProcessError::Capacity);
        }
    }
    rt.session_state
        .append_chat_input(batch)
        .await
        .map_err(storage_error)?;
    repair_projection(rt).await?;
    rt.orchestrator.protect_external_input_from_compaction();
    Ok(())
}
pub(super) fn document(task_id: &str, text: &str) -> super::TeamDocumentResult {
    super::TeamDocumentResult {
        operation_id: format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(task_id, "document")).expect("string pair"))
        ),
        delivery_id: format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(task_id, "document_delivery")).expect("string pair")
            )
        ),
        title: "Shared Bot result".into(),
        markdown: text.into(),
        sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
    }
}

pub(super) async fn record_result(
    rt: &DesktopRuntime,
    task_id: &str,
    result: &super::TeamProcessResult,
) -> Result<(), TeamProcessError> {
    let writer = rt
        .orchestrator
        .session_transcript_writer()
        .ok_or(TeamProcessError::Worker)?;
    let session_id = rt
        .session_state
        .bot_chat_cursor()
        .scope
        .ok_or(TeamProcessError::Worker)?
        .session_id()
        .map_err(storage_error)?;
    let row = serde_json::json!({"type":"system","subtype":"team_bot_result","uuid":uuid(&format!("team-result:{task_id}")),"sessionId":session_id.as_uuid().to_string(),"parentUuid":null,"timestamp":chrono::DateTime::from_timestamp_millis(result.finished_at as i64).ok_or(TeamProcessError::Protocol)?.to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"isSidechain":false,"isMeta":true,"isVisibleInTranscriptOnly":true,"message":{"role":"system","content":""},"taskId":task_id,"result":result});
    writer
        .append_json_once_durable(
            &format!("team-result:{task_id}"),
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&row)
                .map_err(storage_error)?,
        )
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// Read a completed result after lost pipe acknowledgement. This acquires the
/// ordinary writer lease and captured transcript authority; never runs a model.
pub fn recover_result(
    storage_root: &Path,
    scope: &BotChatScope,
    task_id: &str,
) -> Result<Option<super::TeamProcessResult>, TeamProcessError> {
    if !super::protocol::valid_id(task_id) {
        return Err(TeamProcessError::Protocol);
    }
    let id = scope.session_id().map_err(storage_error)?;
    let scope_root = storage_root.join("scopes").join(id.as_uuid().to_string());
    let home = scope_root.join("home");
    if !home.exists() {
        return Ok(None);
    }
    let _lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
        .claim_session_id(&id.to_string(), std::process::id())
        .map_err(storage_error)?;
    let path = session::jsonl::session_path(
        &home,
        &scope_root.join("workspace").to_string_lossy(),
        &id.as_uuid().to_string(),
    );
    if !path.exists() {
        return Ok(None);
    }
    let relative = path.strip_prefix(&home).map_err(storage_error)?.to_owned();
    let (_, bytes) = session::jsonl::writer::TranscriptFileAuthority::capture(home, relative)
        .map_err(storage_error)?;
    let source = std::str::from_utf8(&bytes).map_err(storage_error)?;
    for line in source.lines() {
        let value: serde_json::Value = serde_json::from_str(line).map_err(storage_error)?;
        if value["type"] == "system"
            && value["subtype"] == "team_bot_result"
            && value["taskId"] == task_id
        {
            let result: super::TeamProcessResult =
                serde_json::from_value(value["result"].clone()).map_err(storage_error)?;
            super::protocol::validate_result(&result)?;
            if result.cursor.scope.as_ref() != Some(scope) {
                return Err(TeamProcessError::Protocol);
            }
            return Ok(Some(result));
        }
    }
    Ok(None)
}
