//! Cross-session agent restore — the read side of [`session::agent_rows`].
//!
//! [`FileParkedAgentStore`] records a parked background agent's launch
//! configuration beside its transcript, and [`restore_parked_agents`] rebuilds
//! those agents in a LATER process: it re-spawns each one through the same
//! `BackgroundAgentSpawner` a fresh launch uses, seeded with the conversation
//! recovered from its transcript instead of a prompt.
//!
//! A restore is deliberately narrow. It restores agents that PARKED (a
//! terminal agent has no row), whose transcript still exists (a row without one
//! could only be restarted from scratch, which is not a resume), and — for a
//! forked skill — whose permission scoping still corroborates. The last check
//! is the existing host fork-resume gate, consulted
//! here on its COLD path: there is no live task record in a fresh process, so
//! the on-disk provenance marker is the only witness to the fork's identity.

use async_trait::async_trait;
use lingxi_core::host::fork_resume_gate::ForkResumeGate;
use lingxi_core::host::parked_agent_store::ParkedAgentStore;
use lingxi_core::host::subagent_spawn::{
    SubagentInheritance, SubagentSpawnRequest, SubagentSpawner,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

#[cfg(all(test, feature = "mobile"))]
#[path = "parked_agent_restore_tests.rs"]
mod live_registry_tests;

/// Writes and erases parked-agent rows under this session's `subagents/` dir.
pub struct FileParkedAgentStore {
    /// Where the rows live, beside each agent's transcript and fork sidecars.
    pub subagents_dir: std::path::PathBuf,
}

#[async_trait]
impl ParkedAgentStore for FileParkedAgentStore {
    fn register_origin(
        &self,
        _agent_id: lingxi_core::types::AgentId,
        _request: &SubagentSpawnRequest,
    ) {
        // This store's directory is already pinned by construction.
    }

    async fn park(
        &self,
        task_id: &str,
        agent_id: lingxi_core::types::AgentId,
        description: &Utf16JsonProjection,
        request: &SubagentSpawnRequest,
        handback: Option<&lingxi_core::host::handback::HandbackState>,
        history: &[lingxi_core::host::handback::HandbackState],
    ) {
        let row = session::agent_rows::ParkedAgentRow {
            task_id: task_id.to_string(),
            agent_id,
            description: description.clone(),
            request: request.clone(),
            handback_opt_in: request.handback_opt_in,
            handback_state: handback.cloned(),
            handback_history: history.to_vec(),
        };
        // Best-effort: failing to record a parked agent costs a later restore,
        // never the run in progress.
        if let Err(e) = session::agent_rows::write_row(&self.subagents_dir, &row).await {
            tracing::debug!("could not record parked agent {agent_id}: {e}");
        }
    }

    async fn unpark(&self, agent_id: lingxi_core::types::AgentId) {
        if let Err(e) =
            session::agent_rows::remove_row(&self.subagents_dir, &agent_id.to_string()).await
        {
            tracing::debug!("could not clear parked agent {agent_id}: {e}");
        }
    }
}

/// Pin each parked row to its originating session after the host activates a
/// different conversation. Requests without an origin session use the live
/// provider directory at startup.
pub(crate) struct SessionParkedAgentStore {
    home: std::path::PathBuf,
    cwd: String,
    fallback: std::sync::Arc<dyn Fn() -> Option<std::path::PathBuf> + Send + Sync>,
    directories: std::sync::Mutex<
        std::collections::HashMap<lingxi_core::types::AgentId, std::path::PathBuf>,
    >,
}

impl SessionParkedAgentStore {
    pub(crate) fn new(
        home: std::path::PathBuf,
        cwd: String,
        fallback: std::sync::Arc<dyn Fn() -> Option<std::path::PathBuf> + Send + Sync>,
    ) -> Self {
        Self {
            home,
            cwd,
            fallback,
            directories: Default::default(),
        }
    }

    fn directory_for_origin(
        &self,
        origin: Option<lingxi_core::types::SessionId>,
    ) -> Option<std::path::PathBuf> {
        let current = (self.fallback)();
        let Some(origin) = origin else {
            return current;
        };
        if current.as_ref().is_some_and(|directory| {
            directory.file_name().and_then(|name| name.to_str()) == Some("subagents")
                && directory
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == origin.as_uuid().to_string())
        }) {
            return current;
        }
        Some(orchestrator::transcript_paths::subagents_dir(
            &self.home,
            &self.cwd,
            &origin.as_uuid().to_string(),
        ))
    }
}

#[async_trait]
impl ParkedAgentStore for SessionParkedAgentStore {
    fn register_origin(
        &self,
        agent_id: lingxi_core::types::AgentId,
        request: &SubagentSpawnRequest,
    ) {
        let origin = request.origin_session_id.or_else(|| {
            request
                .restored_handback_state
                .as_ref()
                .map(|state| state.run.scope.session_id)
        });
        let directory = self.directory_for_origin(origin);
        if let Some(directory) = directory {
            self.directories
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(agent_id, directory);
        }
    }

    async fn park(
        &self,
        task_id: &str,
        agent_id: lingxi_core::types::AgentId,
        description: &Utf16JsonProjection,
        request: &SubagentSpawnRequest,
        handback: Option<&lingxi_core::host::handback::HandbackState>,
        history: &[lingxi_core::host::handback::HandbackState],
    ) {
        let directory = self
            .directories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agent_id)
            .cloned();
        let Some(mut subagents_dir) = directory else {
            return;
        };
        // A directory move relocates this conversation's sidecars. A late
        // completion for a different conversation keeps its original owner.
        if let Some(current) = (self.fallback)() {
            if current.parent().and_then(|path| path.file_name())
                == subagents_dir.parent().and_then(|path| path.file_name())
            {
                subagents_dir = current;
            }
        }
        self.directories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent_id, subagents_dir.clone());
        FileParkedAgentStore { subagents_dir }
            .park(task_id, agent_id, description, request, handback, history)
            .await;
    }

    async fn unpark(&self, agent_id: lingxi_core::types::AgentId) {
        let directory = self
            .directories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&agent_id);
        if let Some(subagents_dir) = directory {
            FileParkedAgentStore { subagents_dir }
                .unpark(agent_id)
                .await;
        }
    }
}

/// What a restore attempt concluded for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Re-spawned under the persisted stable agent id.
    Restored(lingxi_core::types::AgentId),
    /// The forked-skill resume gate refused it. Carries the refusal, which is
    /// the same byte-exact message a live resume would surface.
    Refused(String),
    /// Its transcript recovered no messages — nothing to resume INTO, so
    /// re-spawning would silently restart the work from scratch.
    EmptyTranscript,
    /// The spawner could not start it.
    Failed(String),
}

struct RestoredTranscript {
    history: Vec<lingxi_core::types::ConversationMessage>,
    /// Actual announcement payloads, separate from the model projection.
    /// Cold resume reloads files and the lazy Read cursor from current policy;
    /// only these durable rows establish what the model has already seen.
    announcement_history: Vec<Utf16JsonProjection>,
    /// Concrete model/profile recorded by the runner. User and attachment rows
    /// can omit selection; the retained launch configuration then supplies it.
    resolved_selection: Option<(String, Option<String>)>,
}

fn restore_message_with_utf16(
    value: serde_json::Value,
    row: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Option<lingxi_core::types::ConversationMessage> {
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let mut message = serde_json::from_value::<ConversationMessage>(value).ok()?;
    let content = match &mut message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content,
        ConversationMessage::System { content, .. } => {
            if let Some(code_units) = row.string_units("/message/content") {
                let display = String::from_utf16_lossy(&code_units);
                if row.string_at("/message/content") != Some(display.as_str()) {
                    return None;
                }
                *content = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                    display, code_units,
                )
                .ok()?;
            }
            return Some(message);
        }
    };
    for (index, block) in content.iter_mut().enumerate() {
        let pointer = format!("/message/content/{index}/text");
        let Some(code_units) = row.string_units(&pointer) else {
            continue;
        };
        if String::from_utf16(&code_units).is_ok() {
            continue;
        }
        let ContentBlock::Text { text, citations } = block else {
            return None;
        };
        if String::from_utf16_lossy(&code_units) != *text {
            return None;
        }
        *block = ContentBlock::TextJsUtf16 {
            text: text.clone(),
            utf16_code_units: code_units,
            citations: citations.clone(),
        };
    }
    Some(message)
}

fn exact_source_row(
    line: &str,
    overrides: &lingxi_core::types::exact_json::Utf16Overrides,
) -> Result<Utf16JsonProjection, String> {
    let mut row = Utf16JsonProjection::parse(line)
        .map_err(|error| format!("agent transcript row is not valid source JSON: {error}"))?;
    for (pointer, code_units) in overrides {
        let display = String::from_utf16_lossy(code_units);
        if row.string_at(pointer) != Some(display.as_str()) {
            return Err(format!(
                "agent transcript exact UTF-16 field does not match its display projection: {pointer}"
            ));
        }
        row.set_string_units(pointer, code_units).map_err(|error| {
            format!("agent transcript exact UTF-16 fields are invalid: {error}")
        })?;
    }
    Ok(row)
}

/// Reconstruct how typed exact-UTF-16 text appears after Native transcript
/// serialization and parsing. The transcript stores every text block as a
/// regular `text` block; only ill-formed UTF-16 needs the exact sidecar to
/// recover `TextJsUtf16`. A well-formed carrier is equivalent to its decoded
/// scalar text when its display projection agrees.
fn native_transcript_roundtrip_message(
    message: &lingxi_core::types::ConversationMessage,
) -> lingxi_core::types::ConversationMessage {
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let mut message = message.clone();
    let content = match &mut message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content,
        ConversationMessage::System { .. } => return message,
    };
    for block in content {
        let replacement = match block {
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
            } => String::from_utf16(utf16_code_units)
                .ok()
                .filter(|decoded| decoded.as_str() == text.as_str())
                .map(|decoded| ContentBlock::Text {
                    text: decoded,
                    citations: citations.clone(),
                }),
            _ => None,
        };
        if let Some(replacement) = replacement {
            *block = replacement;
        }
    }
    message
}

fn exact_source_attachment_marker(
    message_id: lingxi_core::types::MessageId,
    attachment: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<String, String> {
    let attachment_json = attachment
        .to_json_string()
        .map_err(|error| format!("agent source attachment is invalid: {error}"))?;
    let message_id_json = serde_json::to_string(&message_id)
        .map_err(|error| format!("agent source attachment identity is invalid: {error}"))?;
    Ok(format!(
        "{{\"messageId\":{message_id_json},\"attachment\":{attachment_json}}}"
    ))
}

async fn read_restored_transcript(
    subagents_dir: &std::path::Path,
    agent_id: lingxi_core::types::AgentId,
    origin_session: Option<lingxi_core::types::SessionId>,
) -> Result<RestoredTranscript, String> {
    let path = session::forked_skill::agent_transcript_path(subagents_dir, &agent_id.to_string());
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return Ok(RestoredTranscript {
            history: Vec::new(),
            announcement_history: Vec::new(),
            resolved_selection: None,
        });
    };
    let mut history = Vec::new();
    let mut announcement_history = Vec::new();
    let mut resolved_selection = None;
    let mut explicit_model_selection = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let decoded = lingxi_core::types::exact_json::parse_exact_json(line)
            .map_err(|error| format!("agent transcript is incomplete: {error}"))?;
        let exact_strings = decoded.utf16_overrides.clone();
        let value = decoded.value;
        let row_projection = exact_source_row(line, &exact_strings)?;
        let attachment = value.get("attachment");
        if attachment.is_some_and(|attachment| {
            attachment.get("type").and_then(serde_json::Value::as_str) == Some("subagent_handback")
                || attachment.get("envelope").is_some()
        }) {
            use lingxi_core::host::handback::{HandbackEnvelope, HandbackRecipient};
            let attachment = attachment.unwrap();
            let envelope_source = row_projection
                .subprojection("/attachment/envelope")
                .and_then(|envelope| envelope.to_json_string())
                .map_err(|error| format!("invalid peer report transcript envelope: {error}"))?;
            let envelope: HandbackEnvelope = serde_json::from_str(&envelope_source)
                .map_err(|error| format!("invalid peer report transcript envelope: {error}"))?;
            let expected = envelope.model_message();
            let message = value
                .get("message")
                .cloned()
                .and_then(|message| restore_message_with_utf16(message, &row_projection))
                .ok_or_else(|| {
                    "peer report transcript row is missing or has an invalid model projection"
                        .to_string()
                })?;
            let valid = value.get("type").and_then(serde_json::Value::as_str) == Some("attachment")
                && attachment.get("type").and_then(serde_json::Value::as_str)
                    == Some("subagent_handback")
                && envelope.validate()
                && matches!(envelope.receipt.recipient, HandbackRecipient::Agent { agent_id: target, .. } if target == agent_id)
                && origin_session.is_none_or(|session| envelope.origin.scope.session_id == session)
                && value.get("uuid")
                    == Some(&serde_json::to_value(envelope.receipt.message_id).unwrap())
                && value.get("agent_id") == Some(&serde_json::to_value(agent_id).unwrap())
                && message == native_transcript_roundtrip_message(&expected)
                && decoded.utf16_overrides == envelope.transcript_utf16_overrides();
            if !valid {
                return Err("peer report transcript row lost its recipient, scope, identity, or Peer authority".into());
            }
            history.push(expected);
            announcement_history.push(
                row_projection
                    .subprojection("/attachment")
                    .map_err(|error| format!("invalid peer report source attachment: {error}"))?,
            );
            continue;
        }
        for pointer in exact_strings.keys() {
            let is_message_text = pointer
                .strip_prefix("/message/content/")
                .and_then(|rest| rest.strip_suffix("/text"))
                .is_some_and(|index| index.parse::<usize>().is_ok());
            let is_system_content = pointer == "/message/content";
            let is_source_content = pointer
                .strip_prefix("/source_attachment/content/")
                .is_some_and(|index| index.parse::<usize>().is_ok());
            if !is_message_text && !is_system_content && !is_source_content {
                return Err(format!(
                    "agent transcript exact UTF-16 field is outside a typed message or source attachment: {pointer}"
                ));
            }
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("attachment")
            && value.get("attachment").is_some_and(|attachment| {
                matches!(
                    attachment.get("type").and_then(serde_json::Value::as_str),
                    Some(
                        "instructions"
                            | "session_context"
                            | "date"
                            | "context_sections"
                            | "prompt_snapshot"
                    )
                )
            })
        {
            announcement_history.push(row_projection.subprojection("/attachment").map_err(
                |error| format!("agent transcript source attachment is invalid: {error}"),
            )?);
        }
        let is_model_selection =
            value.get("type").and_then(serde_json::Value::as_str) == Some("model-selection");
        if let Some(model) = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            let profile = value
                .get("model_profile")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|profile| !profile.is_empty())
                .map(str::to_string);
            if is_model_selection || !explicit_model_selection {
                resolved_selection = Some((model.to_string(), profile));
            }
            explicit_model_selection |= is_model_selection;
        }
        if is_model_selection {
            continue;
        }
        if let Some(message) = value
            .get("message")
            .cloned()
            .and_then(|message| restore_message_with_utf16(message, &row_projection))
        {
            // Mod prompt attachments are persisted as source metadata beside
            // the user-visible transcript row. Rebuild a host-only marker so
            // the child prompt attachment restorer can recover provenance;
            // that restorer validates the attachment and removes this marker
            // before any provider request is assembled.
            if let Ok(attachment) = row_projection.subprojection("/source_attachment") {
                let content = exact_source_attachment_marker(message.id(), &attachment)?;
                history.push(lingxi_core::types::ConversationMessage::System {
                    id: lingxi_core::types::MessageId::new(),
                    content: content.into(),
                    subtype: Some("mod_attachment_source".into()),
                    api_system: None,
                    compact_metadata: None,
                    refusal_fallback: None,
                    model_fallback: None,
                });
            }
            history.push(message);
        }
    }
    Ok(RestoredTranscript {
        history,
        announcement_history,
        resolved_selection,
    })
}

/// Rebuild every restorable parked agent found under `subagents_dir`.
///
/// Returns one outcome per row, in the deterministic order
/// [`session::agent_rows::list_restorable`] yields, so a caller can report
/// exactly what happened rather than a count.
///
/// A restored agent's row is REWRITTEN by the handler as soon as it parks
/// again; a refused or failed one keeps its row, so the next process can try
/// again once (for example) the skill it needs is back.
/// The parent mode comes from its current trusted enforcing gate. Every
/// eligible runner starts only after the batch has published its peers.
pub async fn restore_parked_agents(
    subagents_dir: &std::path::Path,
    spawner: &dyn SubagentSpawner,
    gate: &dyn ForkResumeGate,
    inherit: &SubagentInheritance,
    parent_permission_mode: Option<String>,
) -> Vec<(lingxi_core::types::AgentId, RestoreOutcome)> {
    let rows = session::agent_rows::list_restorable(subagents_dir).await;
    let batch =
        lingxi_core::host::handback::HandbackRestoreBatch::new(rows.iter().map(|row| row.agent_id));
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.agent_id;
        // The parked row IS the cold process's durable task record. Pass its
        // fork identity so the gate checks row ↔ scoping equality; ordinary
        // agents still pass `None` and use the marker witness when applicable.
        if let Err(refusal) = gate
            .check_resume(id, row.request.forked_skill_name.as_ref())
            .await
        {
            out.push((id, RestoreOutcome::Refused(refusal)));
            batch.release_failed(id);
            continue;
        }
        let origin_session = row.request.origin_session_id.or_else(|| {
            row.handback_state
                .as_ref()
                .map(|state| state.run.scope.session_id)
        });
        let transcript = match read_restored_transcript(subagents_dir, id, origin_session).await {
            Ok(transcript) => transcript,
            Err(error) => {
                out.push((id, RestoreOutcome::Failed(error)));
                batch.release_failed(id);
                continue;
            }
        };
        if transcript.history.is_empty() {
            out.push((id, RestoreOutcome::EmptyTranscript));
            batch.release_failed(id);
            continue;
        }
        let mut request = row.request.clone();
        request.origin_session_id = origin_session;
        request.handback_opt_in = row.handback_opt_in
            && request.fork_context_messages.is_none()
            && request.forked_skill_name.is_none()
            && request.observer.is_none();
        request.restored_handback_state = row.handback_state.clone();
        request.restored_handback_history = row.handback_history.clone();
        request.parent_permission_mode = parent_permission_mode.clone();
        request.restore_handback_start = batch.participant(id);
        // Transcripts persist the actual wire selection after definition,
        // inheritance, policy and provider routing have resolved. Pin that exact
        // pair on cold resume. Without an observed selection, retain the
        // configured launch model/profile.
        if let Some((model, profile)) = transcript.resolved_selection {
            request.model = Some(model);
            request.model_profile = profile;
        }
        // The recovered conversation REPLACES the seeding — prompt, fork
        // context and preload alike. Re-sending the original prompt would make
        // the agent redo work its own transcript already records, and reusing
        // `fork_context_messages` (a PREFIX the runner adds ahead of the prompt
        // and the `SubagentStart` preload) would additionally re-fire start
        // hooks for a run that began in another process.
        request.resumed_history = Some(transcript.history);
        request.instruction_context = Some(lingxi_core::host::instructions::InstructionContext {
            announcement_history: transcript.announcement_history,
            ..Default::default()
        });
        request.prompt = Utf16JsonProjection::from("");
        match spawner
            .restore_async_task(&row.task_id, id, request, inherit.clone())
            .await
        {
            Ok(launch) => out.push((id, RestoreOutcome::Restored(launch.agent_id))),
            Err(e) => {
                batch.release_failed(id);
                out.push((id, RestoreOutcome::Failed(e.to_string())));
            }
        }
    }
    out
}

/// Mobile shares the persistent LocalAgent handler without the desktop
/// coordinator. Restore through that same registered handler and install the
/// original task alias before releasing its model startup gate.
#[cfg(feature = "mobile")]
pub async fn restore_parked_agents_in_registry(
    subagents_dir: &std::path::Path,
    registry: std::sync::Arc<tasks::registry::TaskRegistry>,
    gate: &dyn ForkResumeGate,
    inherit: &SubagentInheritance,
    parent_permission_mode: Option<String>,
) -> Vec<(lingxi_core::types::AgentId, RestoreOutcome)> {
    struct RegistryRestoreSpawner(std::sync::Arc<tasks::registry::TaskRegistry>);
    #[async_trait]
    impl SubagentSpawner for RegistryRestoreSpawner {
        async fn spawn(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<lingxi_core::host::SubagentResult, lingxi_core::host::SubagentSpawnError>
        {
            Err(lingxi_core::host::SubagentSpawnError::Internal(
                "parked-agent restorer requires a registered task address".into(),
            ))
        }

        async fn restore_async_task(
            &self,
            task_id: &str,
            agent_id: lingxi_core::types::AgentId,
            request: SubagentSpawnRequest,
            inherit: SubagentInheritance,
        ) -> Result<
            lingxi_core::host::subagent_spawn::AsyncLaunch,
            lingxi_core::host::SubagentSpawnError,
        > {
            let restored_task = self
                .0
                .spawn_with_aliases(
                    tasks::TaskType::LocalAgent,
                    tasks::TaskSpawnInput::LocalAgent {
                        agent_id,
                        subagent_type: request.subagent_type.clone(),
                        prompt: request.prompt.clone(),
                        is_backgrounded: true,
                        tool_use_id: request.tool_use_id.clone(),
                        creator_teammate_name: request.creator_teammate_name.clone(),
                        creator_team_name: request.creator_team_name.clone(),
                        creator_agent_id: request.creator_agent_id,
                        spawn_request: Some(request.clone()),
                        inheritance: Some(inherit),
                    },
                    request
                        .description
                        .clone()
                        .unwrap_or_else(|| Utf16JsonProjection::from("")),
                    &[task_id.to_string()],
                )
                .await
                .map_err(|error| {
                    lingxi_core::host::SubagentSpawnError::Runtime(error.to_string())
                })?;
            let output_file = self
                .0
                .output_manager
                .path_for(&restored_task)
                .map_err(|error| lingxi_core::host::SubagentSpawnError::Runtime(error.to_string()))?
                .to_string_lossy()
                .into_owned();
            Ok(lingxi_core::host::subagent_spawn::AsyncLaunch {
                agent_id,
                output_file,
            })
        }
    }
    restore_parked_agents(
        subagents_dir,
        &RegistryRestoreSpawner(registry),
        gate,
        inherit,
        parent_permission_mode,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::subagent_spawn::{
        AsyncLaunch, SelectedAgentMeta, SubagentListingEntry, SubagentResult, SubagentSpawnError,
    };
    use session::agent_rows::{write_row, ParkedAgentRow};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    fn source_string(
        text: impl Into<String>,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        let text = text.into();
        lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            text.clone(),
            text.encode_utf16().collect(),
        )
        .expect("test string is a valid source string")
    }

    fn request() -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            // Restoring an existing parked agent is not a fresh Agent-tool
            // admission and must not create a second spawn-statistics receipt.
            agent_spawn_token: None,
            stop_hook_scope: Default::default(),
            agent_spawn_provenance: Default::default(),
            teammate_color: None,
            subagent_type: "general-purpose".into(),
            prompt: source_string("the original prompt"),
            observer: None,
            context_paths: Vec::new(),
            description: Some("research".into()),
            model: Some("claude-opus-5".into()),
            model_profile: None,
            run_in_background: true,
            name: None,
            team_name: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            mode: None,
            isolation: None,
            cwd: Some("/repo".into()),
            worktree: None,
            fork_context_messages: None,
            instruction_context: None,
            fork_parent_system_prompt: None,
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: None,
            tool_use_id: None,
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            depth: 1,
            origin_session_id: None,
            parent_model_override: None,
            parent_model_profile_override: None,
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            handback_opt_in: false,
            parent_permission_mode: None,
            handback_enabled: None,
            restored_handback_state: None,
            restored_handback_history: Vec::new(),
            handback_ends_turn_enabled: None,
            restore_handback_start: None,
            parent_native_query_depth: None,
        }
    }

    #[derive(Default)]
    struct RecordingSpawner {
        seen: StdMutex<Vec<SubagentSpawnRequest>>,
        fail: bool,
    }
    #[async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(
            &self,
            _r: SubagentSpawnRequest,
            _i: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            unreachable!("restore uses the async path")
        }
        async fn agent_listing(&self) -> Vec<SubagentListingEntry> {
            vec![]
        }
        async fn resolve_selection(
            &self,
            t: &str,
            _m: Option<&str>,
            _p: Option<&str>,
        ) -> SelectedAgentMeta {
            SelectedAgentMeta {
                agent_type: t.to_string(),
                ..Default::default()
            }
        }
        async fn spawn_async(
            &self,
            request: SubagentSpawnRequest,
            _i: SubagentInheritance,
        ) -> Result<AsyncLaunch, SubagentSpawnError> {
            self.seen.lock().unwrap().push(request);
            if self.fail {
                return Err(SubagentSpawnError::Runtime("pool full".into()));
            }
            Ok(AsyncLaunch {
                agent_id: lingxi_core::types::AgentId::new(),
                output_file: "/tmp/a.output".into(),
            })
        }

        async fn restore_async(
            &self,
            agent_id: lingxi_core::types::AgentId,
            request: SubagentSpawnRequest,
            _i: SubagentInheritance,
        ) -> Result<AsyncLaunch, SubagentSpawnError> {
            self.seen.lock().unwrap().push(request);
            if self.fail {
                return Err(SubagentSpawnError::Runtime("pool full".into()));
            }
            Ok(AsyncLaunch {
                agent_id,
                output_file: "/tmp/a.output".into(),
            })
        }
    }

    struct Gate(Option<&'static str>);
    #[async_trait]
    impl ForkResumeGate for Gate {
        async fn check_resume(
            &self,
            _agent_id: lingxi_core::types::AgentId,
            _task_forked_skill_name: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        ) -> Result<(), String> {
            match self.0 {
                Some(msg) => Err(msg.to_string()),
                None => Ok(()),
            }
        }
    }

    struct NoInvoker;
    #[async_trait]
    impl lingxi_core::host::tool_invoker::ToolInvoker for NoInvoker {
        async fn invoke(
            &self,
            _n: &str,
            _i: lingxi_core::types::utf16_json::Utf16JsonProjection,
            _c: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<
            lingxi_core::types::utf16_json::Utf16JsonProjection,
            lingxi_core::host::tool_invoker::ToolInvokerError,
        > {
            Ok(lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null").unwrap())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct NoBudget;
    #[async_trait]
    impl lingxi_core::host::budget::BudgetEnforcerHandle for NoBudget {
        async fn check_and_charge(
            &self,
            _n: u64,
        ) -> Result<(), lingxi_core::host::budget::BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    fn inherit() -> SubagentInheritance {
        SubagentInheritance {
            tool_invoker: Arc::new(NoInvoker),
            budget: Arc::new(NoBudget),
        }
    }

    async fn seed(dir: &std::path::Path, id: lingxi_core::types::AgentId, transcript: &[&str]) {
        write_row(
            dir,
            &ParkedAgentRow {
                task_id: "a00000001".into(),
                agent_id: id,
                description: "research".into(),
                request: request(),
                handback_opt_in: false,
                handback_state: None,
                handback_history: Vec::new(),
            },
        )
        .await
        .unwrap();
        if transcript.is_empty() {
            // Still create the file — an EMPTY transcript is distinct from a
            // missing one (the row is listed, then rejected for having nothing
            // to resume into).
            tokio::fs::write(
                session::forked_skill::agent_transcript_path(dir, &id.to_string()),
                "",
            )
            .await
            .unwrap();
            return;
        }
        let mut body = String::new();
        for t in transcript {
            let msg = lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                (*t).to_string(),
            );
            body.push_str(
                &serde_json::to_string(&serde_json::json!({
                    "agent_id": id.to_string(),
                    "timestamp": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
                    "message": msg,
                }))
                .unwrap(),
            );
            body.push('\n');
        }
        tokio::fs::write(
            session::forked_skill::agent_transcript_path(dir, &id.to_string()),
            body,
        )
        .await
        .unwrap();
    }

    /// The end-to-end point of the whole layer: an agent parked in one process
    /// is rebuilt in the next, seeded with its RECOVERED conversation rather
    /// than its original prompt — otherwise it would redo work its own
    /// transcript already records.
    #[tokio::test]
    async fn a_parked_agent_is_rebuilt_from_its_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &["first", "second"]).await;

        let spawner = RecordingSpawner::default();
        let outcomes =
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, RestoreOutcome::Restored(id));
        let seen = spawner.seen.lock().unwrap();
        let req = seen.first().expect("spawned");
        assert_eq!(
            req.resumed_history.as_ref().map(Vec::len),
            Some(2),
            "seeded with the recovered conversation"
        );
        assert!(
            req.fork_context_messages.is_none(),
            "a restore must NOT ride the fork-context prefix — that would \
             re-run the preload and re-fire SubagentStart"
        );
        assert!(
            req.prompt.string_at("") == Some(""),
            "the original prompt is NOT re-sent: {:?}",
            req.prompt
        );
        // The launch configuration is the row's, not a default.
        assert_eq!(req.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(req.cwd.as_deref(), Some("/repo"));
        assert_eq!(req.depth, 1);
    }

    #[tokio::test]
    async fn cold_restore_recreates_source_attachment_marker_from_transcript_metadata() {
        use lingxi_core::types::{ConversationMessage, MessageId};

        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let message = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>\ntool.call hook additional context: one\n</system-reminder>".into(),
        );
        let attachment = serde_json::json!({
            "type": "hook_additional_context",
            "content": ["one"],
            "hookName": "tool.call",
            "toolUseID": "toolu_1-context",
            "hookEvent": "PostToolUse",
        });
        let transcript = session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
        tokio::fs::write(
            transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "message": message,
                    "source_attachment": attachment,
                })
            ),
        )
        .await
        .unwrap();

        let restored = read_restored_transcript(dir.path(), id, None)
            .await
            .unwrap();
        assert_eq!(restored.history.len(), 2);
        let ConversationMessage::System {
            content,
            subtype: Some(subtype),
            ..
        } = &restored.history[0]
        else {
            panic!("source metadata must become an internal system marker")
        };
        assert_eq!(subtype, "mod_attachment_source");
        let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse_utf16_json(
            &content.string_units("").unwrap(),
        )
        .unwrap();
        let message_id = message.id().as_uuid().to_string();
        assert_eq!(source.string_at("/messageId"), Some(message_id.as_str()));
        assert_eq!(
            source.subprojection("/attachment").unwrap(),
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&attachment)
                .unwrap()
        );
        assert_eq!(restored.history[1], message);
    }

    #[tokio::test]
    async fn cold_restore_rehydrates_exact_tool_context_message_and_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let agent_id = lingxi_core::types::AgentId::new();
        let mut message_units = "<system-reminder>\ntool.call hook additional context: ctx"
            .encode_utf16()
            .collect::<Vec<_>>();
        message_units.push(0xd800);
        message_units.extend("\n</system-reminder>".encode_utf16());
        let message = lingxi_core::types::ConversationMessage::user_meta_js_utf16(
            lingxi_core::types::MessageId::new(),
            String::from_utf16_lossy(&message_units),
            message_units.clone(),
        );
        let mut attachment_units = "ctx".encode_utf16().collect::<Vec<_>>();
        attachment_units.push(0xd800);
        let root_text = |units: Vec<u16>| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&units),
                units,
            )
            .unwrap()
        };
        let attachment = lingxi_core::types::utf16_json::Utf16JsonProjection::object(vec![
            (
                "type".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::from(
                    "hook_additional_context",
                ),
            ),
            (
                "content".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::array(vec![root_text(
                    attachment_units.clone(),
                )])
                .unwrap(),
            ),
            (
                "hookName".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("tool.call"),
            ),
            (
                "toolUseID".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("toolu_cold-context"),
            ),
            (
                "hookEvent".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("PostToolUse"),
            ),
        ])
        .unwrap();
        let mut wire_message = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
            &serde_json::to_string(&lingxi_core::types::ConversationMessage::user_meta(
                message.id(),
                String::from_utf16_lossy(&message_units),
            ))
            .unwrap(),
        )
        .unwrap();
        wire_message
            .replace_pointer_projection("/content/0/text", root_text(message_units.clone()))
            .unwrap();
        let row = lingxi_core::types::utf16_json::Utf16JsonProjection::object(vec![
            (
                "agent_id".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::from(agent_id.to_string()),
            ),
            (
                "timestamp".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    r#"{"secs_since_epoch":0,"nanos_since_epoch":0}"#,
                )
                .unwrap(),
            ),
            ("message".into(), wire_message),
            ("source_attachment".into(), attachment),
        ])
        .unwrap();
        let transcript =
            session::forked_skill::agent_transcript_path(dir.path(), &agent_id.to_string());
        tokio::fs::write(transcript, format!("{}\n", row.to_json_string().unwrap()))
            .await
            .unwrap();

        let restored = read_restored_transcript(dir.path(), agent_id, None)
            .await
            .unwrap();

        assert_eq!(restored.history.len(), 2);
        let lingxi_core::types::ConversationMessage::System {
            content,
            subtype: Some(subtype),
            ..
        } = &restored.history[0]
        else {
            panic!("source attachment marker should precede its reminder");
        };
        assert_eq!(subtype, "mod_attachment_source");
        let marker = lingxi_core::types::utf16_json::Utf16JsonProjection::parse_utf16_json(
            &content.string_units("").unwrap(),
        )
        .unwrap();
        let source = marker.subprojection("/attachment").unwrap();
        assert_eq!(source.string_units("/content/0"), Some(attachment_units));
        let lingxi_core::types::ConversationMessage::User { content, .. } = &restored.history[1]
        else {
            panic!("restored reminder should remain a user message");
        };
        let lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } = &content[0]
        else {
            panic!("restored reminder should retain its exact UTF-16 payload");
        };
        assert_eq!(utf16_code_units, &message_units);
    }

    #[tokio::test]
    async fn cold_restore_keeps_durable_announcements_without_frozen_instruction_state() {
        use lingxi_core::host::instructions::InstructionContext;
        use lingxi_core::types::{ConversationMessage, MessageId};
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &["already completed setup"]).await;
        let mut launch = request();
        launch.instruction_context = Some(InstructionContext {
            user_context: lingxi_core::types::utf16_json::Utf16JsonProjection::object(vec![(
                "claudeMd".into(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("obsolete policy body"),
            )])
            .unwrap(),
            sent_paths: [std::path::PathBuf::from("/repo/AGENTS.md")].into(),
            managed_instructions_only: true,
            ..Default::default()
        });
        write_row(
            dir.path(),
            &ParkedAgentRow {
                task_id: "a00000001".into(),
                agent_id: id,
                description: "research".into(),
                request: launch,
                handback_opt_in: false,
                handback_state: None,
                handback_history: Vec::new(),
            },
        )
        .await
        .unwrap();
        let announcements = [
            json!({"type":"prompt_snapshot","systemPrompt":["captured static prompt"],"contextRendering":"inline"}),
            json!({"type":"instructions","files":[{"path":"/repo/AGENTS.md","type":"Project","content":"previous instructions"}]}),
            json!({"type":"date","date":"2026-09-30"}),
            json!({"type":"session_context","context":{"gitStatus":"clean"}}),
            json!({"type":"context_sections","sections":[{"name":"Environment","text":"custom context"}]}),
            json!({"type":"instructions","files":[],"removed":["/repo/AGENTS.md"],"changed":true}),
        ];
        let transcript_path =
            session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
        let mut rows = tokio::fs::read_to_string(&transcript_path).await.unwrap();
        for attachment in announcements.iter().chain(std::iter::once(&json!({
            "type":"hook_additional_context","content":["a lazy Read frame"]
        }))) {
            let projected = (attachment["type"] != "prompt_snapshot").then(|| {
                ConversationMessage::user_meta(MessageId::new(), "projected reminder".into())
            });
            rows.push_str(
                &serde_json::to_string(&json!({
                    "type":"attachment","attachment":attachment,"message":projected
                }))
                .unwrap(),
            );
            rows.push('\n');
        }
        tokio::fs::write(&transcript_path, rows).await.unwrap();

        let spawner = RecordingSpawner::default();
        let outcomes =
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;
        assert_eq!(outcomes[0].1, RestoreOutcome::Restored(id));
        let seen = spawner.seen.lock().unwrap();
        let restored = seen[0].instruction_context.as_ref().unwrap();
        let expected_announcements = announcements
            .iter()
            .map(|attachment| {
                lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(attachment)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(restored.announcement_history, expected_announcements);
        assert!(restored.user_context_is_empty());
        assert!(restored.sent_paths.is_empty());
        assert!(!restored.managed_instructions_only);
        assert_eq!(seen[0].resumed_history.as_ref().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn restore_pins_the_resolved_transcript_model_and_profile() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_row(
            dir.path(),
            &ParkedAgentRow {
                task_id: "a00000001".into(),
                agent_id: id,
                description: "research".into(),
                request: request(),
                handback_opt_in: false,
                handback_state: None,
                handback_history: Vec::new(),
            },
        )
        .await
        .unwrap();
        let message = lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "already completed setup".to_string(),
        );
        let entry = serde_json::json!({
            "agent_id": id.to_string(),
            "timestamp": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "message": message,
            "model": "deepseek-flash",
            "model_profile": "deepseek"
        });
        tokio::fs::write(
            session::forked_skill::agent_transcript_path(dir.path(), &id.to_string()),
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .await
        .unwrap();

        let spawner = RecordingSpawner::default();
        let outcomes =
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;

        assert_eq!(outcomes[0].1, RestoreOutcome::Restored(id));
        let seen = spawner.seen.lock().unwrap();
        let restored = seen.first().expect("restored request");
        assert_eq!(restored.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(restored.model_profile.as_deref(), Some("deepseek"));
    }

    #[tokio::test]
    async fn restore_uses_latest_committed_model_selection_over_stale_message_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &["already completed setup"]).await;
        let path = session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
        let mut rows = tokio::fs::read_to_string(&path).await.unwrap();
        for selection in [
            serde_json::json!({"type":"model-selection","model":"shared-model","model_profile":"b"}),
            serde_json::json!({"model":"old-model","model_profile":"a"}),
            serde_json::json!({"type":"model-selection","model":"shared-model","model_profile":null}),
            serde_json::json!({"model":"shared-model","model_profile":"b"}),
        ] {
            rows.push_str(&selection.to_string());
            rows.push('\n');
        }
        tokio::fs::write(path, rows).await.unwrap();
        let spawner = RecordingSpawner::default();
        let outcomes =
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;
        assert_eq!(outcomes[0].1, RestoreOutcome::Restored(id));
        let seen = spawner.seen.lock().unwrap();
        assert_eq!(seen[0].model.as_deref(), Some("shared-model"));
        assert_eq!(seen[0].model_profile, None);
        assert_eq!(seen[0].resumed_history.as_ref().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn transcript_without_model_metadata_keeps_the_retained_launch_selection() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &["message without model metadata"]).await;

        let spawner = RecordingSpawner::default();
        restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;

        let seen = spawner.seen.lock().unwrap();
        let restored = seen.first().expect("restored request");
        assert_eq!(restored.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(restored.model_profile, None);
    }

    /// The forked-skill gate is consulted on its COLD path and its refusal is
    /// surfaced verbatim — a restore must not become a way to resume a fork
    /// whose scoping no longer corroborates.
    #[tokio::test]
    async fn a_refused_fork_is_not_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &["first"]).await;

        let spawner = RecordingSpawner::default();
        let outcomes = restore_parked_agents(
            dir.path(),
            &spawner,
            &Gate(Some(
                "refusing to resume it without the skill's permission scoping.",
            )),
            &inherit(),
            None,
        )
        .await;

        assert_eq!(
            outcomes[0].1,
            RestoreOutcome::Refused(
                "refusing to resume it without the skill's permission scoping.".into()
            )
        );
        assert!(
            spawner.seen.lock().unwrap().is_empty(),
            "a refused agent is never spawned"
        );
    }

    /// An empty transcript means there is nothing to resume INTO; re-spawning
    /// would silently restart the work from scratch.
    #[tokio::test]
    async fn an_empty_transcript_is_not_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &[]).await;

        let spawner = RecordingSpawner::default();
        let outcomes =
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;
        assert_eq!(outcomes[0].1, RestoreOutcome::EmptyTranscript);
        assert!(spawner.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_spawn_failure_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), lingxi_core::types::AgentId::new(), &["first"]).await;
        let spawner = RecordingSpawner {
            fail: true,
            ..Default::default()
        };
        let outcomes =
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;
        assert!(matches!(outcomes[0].1, RestoreOutcome::Failed(_)));
    }

    /// park writes a row, unpark erases it — and the ABSENCE is what stops a
    /// terminated agent from being revived.
    #[tokio::test]
    async fn park_then_unpark_leaves_nothing_to_restore() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let store = FileParkedAgentStore {
            subagents_dir: dir.path().to_path_buf(),
        };
        store
            .park(
                "a00000001",
                id,
                &source_string("research"),
                &request(),
                None,
                &[],
            )
            .await;
        assert!(session::agent_rows::read_row(dir.path(), &id.to_string())
            .await
            .is_some());

        store.unpark(id).await;
        assert!(session::agent_rows::read_row(dir.path(), &id.to_string())
            .await
            .is_none());

        let spawner = RecordingSpawner::default();
        assert!(
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn nothing_parked_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let spawner = RecordingSpawner::default();
        assert!(
            restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cold_restore_retains_full_report_and_eligibility_separately_from_launch_json() {
        use lingxi_core::host::handback::*;
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &[HANDBACK_REMINDER]).await;
        let scope = HandbackSessionScope {
            session_id: lingxi_core::types::SessionId::new(),
            activation_epoch: 7,
        };
        let run = HandbackRunKey {
            scope,
            agent_id: id,
            run_epoch: 4,
        };
        let recipient = HandbackRecipient::Agent {
            scope,
            agent_id: lingxi_core::types::AgentId::new(),
        };
        let mut state =
            HandbackState::new_run(run, true, None, Some(recipient), recipient, |_| true);
        state.report = Some(HandbackReport::from_utf8_text(
            "the whole sanitized report beyond its inbox pointer",
            Some("stored report warning".into()),
        ));
        state.receipt = Some(HandbackReceipt {
            run,
            recipient,
            message_id: lingxi_core::types::MessageId::new(),
        });
        state.disposition = Some(HandbackDisposition::Flagged);
        state.bounce_count = 3;
        let mut launch = request();
        launch.handback_opt_in = true;
        let store = FileParkedAgentStore {
            subagents_dir: dir.path().to_path_buf(),
        };
        store
            .park(
                "a00000001",
                id,
                &source_string("research"),
                &launch,
                Some(&state),
                &[],
            )
            .await;
        let row = session::agent_rows::read_row(dir.path(), &id.to_string())
            .await
            .unwrap();
        assert!(
            !row.request.handback_opt_in,
            "serialized launch does not mint report authority"
        );
        assert!(row.handback_opt_in);
        assert_eq!(row.handback_state, Some(state.clone()));
        let spawner = RecordingSpawner::default();
        restore_parked_agents(
            dir.path(),
            &spawner,
            &Gate(None),
            &inherit(),
            Some("plan".into()),
        )
        .await;
        let requests = spawner.seen.lock().unwrap();
        assert!(requests[0].handback_opt_in);
        assert_eq!(requests[0].parent_permission_mode.as_deref(), Some("plan"));
        assert_eq!(requests[0].restored_handback_state, Some(state));
    }

    #[tokio::test]
    async fn explicit_opt_out_is_retained_by_shared_restore() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        seed(dir.path(), id, &["ordinary opted-out history"]).await;
        let spawner = RecordingSpawner::default();
        restore_parked_agents(
            dir.path(),
            &spawner,
            &Gate(None),
            &inherit(),
            Some("auto".into()),
        )
        .await;
        assert!(!spawner.seen.lock().unwrap()[0].handback_opt_in);
    }

    #[tokio::test]
    async fn parked_report_keeps_its_origin_directory_after_session_switch() {
        let root = tempfile::tempdir().unwrap();
        let fallback = root.path().join("new-session");
        let store = SessionParkedAgentStore::new(
            root.path().to_path_buf(),
            "/workspace".into(),
            Arc::new({
                let fallback = fallback.clone();
                move || Some(fallback.clone())
            }),
        );
        let id = lingxi_core::types::AgentId::new();
        let mut launch = request();
        let origin = lingxi_core::types::SessionId::new();
        launch.origin_session_id = Some(origin);
        let original = orchestrator::transcript_paths::subagents_dir(
            root.path(),
            "/workspace",
            &origin.as_uuid().to_string(),
        );
        store.register_origin(id, &launch);
        store
            .park(
                "original-task",
                id,
                &source_string("research"),
                &launch,
                None,
                &[],
            )
            .await;
        assert!(session::agent_rows::read_row(&original, &id.to_string())
            .await
            .is_some());
        assert!(!fallback.exists());
        store.unpark(id).await;
        assert!(session::agent_rows::read_row(&original, &id.to_string())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn startup_registration_removes_cold_row_after_session_switch_before_first_rest() {
        for trusted_origin in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let id = lingxi_core::types::AgentId::new();
            let origin = lingxi_core::types::SessionId::new();
            let original = if trusted_origin {
                orchestrator::transcript_paths::subagents_dir(
                    root.path(),
                    "/resumed-workspace",
                    &origin.as_uuid().to_string(),
                )
            } else {
                root.path().join("standalone-original-session")
            };
            let current = Arc::new(std::sync::RwLock::new(original.clone()));
            let store = SessionParkedAgentStore::new(
                root.path().to_path_buf(),
                "/workspace".into(),
                Arc::new({
                    let current = current.clone();
                    move || Some(current.read().unwrap().clone())
                }),
            );
            let mut launch = request();
            launch.origin_session_id = trusted_origin.then_some(origin);
            FileParkedAgentStore {
                subagents_dir: original.clone(),
            }
            .park(
                "cold-task",
                id,
                &source_string("cold retained row"),
                &launch,
                None,
                &[],
            )
            .await;

            // This is the startup callback, without a new park in this process.
            store.register_origin(id, &launch);
            let next = root.path().join("new-session");
            *current.write().unwrap() = next.clone();
            FileParkedAgentStore {
                subagents_dir: next.clone(),
            }
            .park(
                "other-session-task",
                id,
                &source_string("leave current directory alone"),
                &request(),
                None,
                &[],
            )
            .await;
            store.unpark(id).await;
            assert!(session::agent_rows::read_row(&original, &id.to_string())
                .await
                .is_none());
            assert!(session::agent_rows::read_row(&next, &id.to_string())
                .await
                .is_some());
            store.unpark(id).await;
            assert!(
                session::agent_rows::read_row(&next, &id.to_string())
                    .await
                    .is_some(),
                "repeated terminal cleanup must not fall back to the current session"
            );
        }
    }

    #[tokio::test]
    async fn cold_nested_report_replays_only_valid_peer_meta_and_rejects_authority_mutation() {
        use lingxi_core::host::handback::*;
        for mutation in [
            "valid",
            "utf16-valid",
            "utf16-display-forged",
            "utf16-escaped-body-forged",
            "utf16-escaped-message-forged",
            "utf16-sidecar-lossy-scalar",
            "human-message",
            "human-origin",
            "wrong-recipient",
            "wrong-session",
            "wrong-id",
            "changed-body",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let id = lingxi_core::types::AgentId::new();
            seed(dir.path(), id, &["original child context"]).await;
            let scope = HandbackSessionScope {
                session_id: lingxi_core::types::SessionId::new(),
                activation_epoch: 2,
            };
            let mut row = session::agent_rows::read_row(dir.path(), &id.to_string())
                .await
                .unwrap();
            row.request.origin_session_id = Some(scope.session_id);
            write_row(dir.path(), &row).await.unwrap();
            let envelope = PreparedHandbackReport {
                message_id: lingxi_core::types::MessageId::new(),
                report: HandbackReport::from_utf8_text("nested peer report", None),
                body: source_string(handback_frame("nested peer report")),
                sender_name: "nested".into(),
                sender_id: "nested".into(),
                sender_task_id: "nested-task".into(),
                agent_type: "reviewer".into(),
                flagged: false,
            }
            .envelope(
                HandbackRunKey {
                    scope,
                    agent_id: lingxi_core::types::AgentId::new(),
                    run_epoch: 1,
                },
                HandbackRecipient::Agent {
                    scope,
                    agent_id: id,
                },
            );
            let mut envelope = envelope;
            if mutation.starts_with("utf16-") {
                let mut units: Vec<u16> = envelope.body.string_units("").unwrap();
                units.push(0xD800);
                envelope.body = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                    String::from_utf16_lossy(&units),
                    units,
                )
                .unwrap();
            }
            let message = envelope.model_message();
            let mut transcript_message = message.clone();
            if let lingxi_core::types::ConversationMessage::User { content, .. } =
                &mut transcript_message
            {
                for content_block in content {
                    let replacement = match content_block {
                        lingxi_core::types::ContentBlock::TextJsUtf16 {
                            text, citations, ..
                        } => Some(lingxi_core::types::ContentBlock::Text {
                            text: text.clone(),
                            citations: citations.clone(),
                        }),
                        _ => None,
                    };
                    if let Some(replacement) = replacement {
                        *content_block = replacement;
                    }
                }
            }
            // `serde_json::Value` cannot represent a lone surrogate. Build its
            // ordinary display tree from safe message/envelope clones, then let
            // the exact JSON writer restore the original strings.
            let mut display_envelope = envelope.clone();
            let body_units = display_envelope.body.string_units("").unwrap();
            let display_body = String::from_utf16_lossy(&body_units);
            display_envelope.body =
                lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                    display_body.clone(),
                    display_body.encode_utf16().collect(),
                )
                .unwrap();
            let sanitized_message = display_envelope.model_message();
            let mut value = serde_json::json!({
                "type":"attachment", "agent_id":id, "uuid":envelope.receipt.message_id,
                "message":transcript_message,
                "attachment":{"type":"subagent_handback", "envelope":display_envelope},
            });
            let mut exact_overrides = envelope.transcript_utf16_overrides();
            match mutation {
                "utf16-display-forged" => {
                    let original = value["message"]["content"][0]["text"]
                        .as_str()
                        .expect("display projection keeps ordinary text")
                        .to_owned();
                    let forged = original.replacen('\u{FFFD}', "\u{FFFC}", 1);
                    assert_ne!(original, forged, "fixture must display the lone surrogate");
                    value["message"]["content"][0]["text"] = forged.into();
                    assert!(exact_overrides.remove("/message/content/0/text").is_some());
                }
                "utf16-escaped-body-forged" => {
                    *exact_overrides
                        .get_mut("/attachment/envelope/body")
                        .unwrap()
                        .last_mut()
                        .unwrap() = 0xD801;
                }
                "utf16-escaped-message-forged" => {
                    let units = exact_overrides.get_mut("/message/content/0/text").unwrap();
                    *units.iter_mut().find(|unit| **unit == 0xD800).unwrap() = 0xD801;
                }
                "utf16-sidecar-lossy-scalar" => exact_overrides.clear(),
                "human-message" => value["message"]["is_meta"] = false.into(),
                "human-origin" => {
                    value["attachment"]["envelope"]["origin"]["kind"] = "human".into()
                }
                "wrong-recipient" => {
                    value["attachment"]["envelope"]["receipt"]["recipient"]["agent_id"] =
                        serde_json::to_value(lingxi_core::types::AgentId::new()).unwrap()
                }
                "wrong-session" => {
                    let other = serde_json::to_value(lingxi_core::types::SessionId::new()).unwrap();
                    value["attachment"]["envelope"]["receipt"]["run"]["scope"]["session_id"] =
                        other.clone();
                    value["attachment"]["envelope"]["receipt"]["recipient"]["scope"]
                        ["session_id"] = other.clone();
                    value["attachment"]["envelope"]["origin"]["scope"]["session_id"] = other;
                }
                "wrong-id" => {
                    value["uuid"] =
                        serde_json::to_value(lingxi_core::types::MessageId::new()).unwrap()
                }
                "changed-body" => value["attachment"]["envelope"]["body"] = "forged report".into(),
                _ => {}
            }
            let path = session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
            let mut text = tokio::fs::read_to_string(&path).await.unwrap();
            let encoded =
                lingxi_core::types::exact_json::to_vec_with_overrides(&value, &exact_overrides)
                    .unwrap();
            text.push_str(std::str::from_utf8(&encoded).unwrap());
            text.push('\n');
            tokio::fs::write(path, text).await.unwrap();
            let spawner = RecordingSpawner::default();
            let outcomes =
                restore_parked_agents(dir.path(), &spawner, &Gate(None), &inherit(), None).await;
            if mutation == "valid"
                || mutation == "utf16-valid"
                || mutation == "utf16-sidecar-lossy-scalar"
            {
                assert!(
                    matches!(outcomes[0].1, RestoreOutcome::Restored(_)),
                    "{mutation}: {:?}",
                    outcomes[0].1
                );
                let requests = spawner.seen.lock().unwrap();
                let expected_message = if mutation == "utf16-sidecar-lossy-scalar" {
                    let lingxi_core::types::ConversationMessage::User { content, .. } =
                        &sanitized_message
                    else {
                        panic!("sanitized handback remains a user message");
                    };
                    let Some(lingxi_core::types::ContentBlock::TextJsUtf16 {
                        utf16_code_units,
                        ..
                    }) = content.first()
                    else {
                        panic!("sanitized handback retains its UTF-16 display carrier");
                    };
                    assert!(utf16_code_units.contains(&0xfffd));
                    assert!(!utf16_code_units.contains(&0xd800));
                    &sanitized_message
                } else {
                    &message
                };
                assert_eq!(
                    requests[0].resumed_history.as_ref().unwrap().last(),
                    Some(expected_message)
                );
                assert!(requests[0]
                    .instruction_context
                    .as_ref()
                    .unwrap()
                    .announcement_history
                    .iter()
                    .any(|attachment| {
                        attachment
                            .get_property("type")
                            .and_then(|value| value.as_str())
                            == Some("subagent_handback")
                    }));
            } else {
                assert!(
                    matches!(outcomes[0].1, RestoreOutcome::Failed(_)),
                    "{mutation}"
                );
                assert!(
                    spawner.seen.lock().unwrap().is_empty(),
                    "{mutation} must never start a model"
                );
            }
        }
    }
}
