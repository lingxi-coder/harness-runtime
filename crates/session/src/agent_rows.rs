//! Durable rows for PARKED background agents — the half of cross-session
//! resume that survives the process.
//!
//! A backgrounded `local_agent` lives in three places: its conversation (the
//! per-agent transcript, `agent-<id>.jsonl`), its permission scoping when it is
//! a forked skill (`agent-<id>.forked-skill.json`), and its LAUNCH
//! CONFIGURATION — model, cwd, isolation, depth, agent type. The first two are
//! already on disk; the third lived only in `TaskRegistry`'s in-memory map, so
//! a restarted process could read what an agent said and had no idea how to
//! start it again.
//!
//! This module is that third file, `agent-<id>.task.json`, written beside the
//! other two so ONE directory holds everything about one agent — and a stale
//! row is trivially detectable, because a row whose transcript is gone
//! describes an agent that cannot be reconstructed.
//!
//! ## When a row exists
//!
//! Written when a persistent agent comes to REST and REMOVED when it reaches a
//! terminal state. Rest is the only point a resume can target (the agent is
//! parked between turn-sets with a complete transcript), and deleting on
//! terminal means a restore can never revive a finished agent — the absence of
//! a row IS the "do not restore" signal, rather than a status field a reader
//! could forget to check.

use std::path::{Path, PathBuf};

use lingxi_core::host::subagent_spawn::SubagentSpawnRequest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Maximum on-disk size of a row. A row is a few KB; anything approaching this
/// is not a row we wrote, so it is rejected unread.
pub const ROW_MAX_BYTES: u64 = 1_048_576;

/// A parked background agent's launch configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedAgentRow {
    /// The registry task id (`a………`) the agent was known by.
    pub task_id: String,
    /// The agent id its transcript and sidecars are keyed on.
    pub agent_id: lingxi_core::types::AgentId,
    /// Human-readable description (the task's).
    pub description: String,
    /// The full spawn request, so the rebuilt runner is configured exactly as
    /// the original was — model, cwd, isolation, depth, tool overrides. This is
    /// the field that makes a restore faithful rather than approximate.
    pub request: SubagentSpawnRequest,
    /// Eligibility selected by the ordinary Agent host. The request omits this
    /// capability during serde, so cold restore retains it in the trusted row.
    pub handback_opt_in: bool,
    /// Mutable reporting state at the latest rest. It is separate from the
    /// original request because the recipient may change during the run.
    pub handback_state: Option<lingxi_core::host::handback::HandbackState>,
    pub handback_history: Vec<lingxi_core::host::handback::HandbackState>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredParkedAgentRow {
    #[serde(rename = "taskId")]
    task_id: String,
    #[serde(rename = "agentId")]
    agent_id: lingxi_core::types::AgentId,
    description: String,
    handback_opt_in: bool,
    handback_payload: ParkedPayloadRef,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParkedPayloadRef {
    version: u8,
    file: String,
    sha256: String,
    origin_session_id: Option<lingxi_core::types::SessionId>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParkedAgentPayload {
    version: u8,
    agent_id: lingxi_core::types::AgentId,
    origin_session_id: Option<lingxi_core::types::SessionId>,
    request: SubagentSpawnRequest,
    handback_state: Option<lingxi_core::host::handback::HandbackState>,
    handback_history: Vec<lingxi_core::host::handback::HandbackState>,
}

fn payload_file_name(agent_id: lingxi_core::types::AgentId, digest: &str) -> String {
    format!("agent-{agent_id}.handback-{digest}.json")
}

fn directory_session(directory: &Path) -> Option<lingxi_core::types::SessionId> {
    if directory.file_name()?.to_str()? != "subagents" {
        return None;
    }
    let session = uuid::Uuid::parse_str(directory.parent()?.file_name()?.to_str()?).ok()?;
    Some(lingxi_core::types::SessionId::from_uuid(session))
}

fn valid_payload_ref(agent_id: lingxi_core::types::AgentId, reference: &ParkedPayloadRef) -> bool {
    reference.version == 1
        && reference.sha256.len() == 64
        && reference
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && reference.file == payload_file_name(agent_id, &reference.sha256)
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = async {
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn stored_row_at(path: &Path) -> Option<StoredParkedAgentRow> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > ROW_MAX_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    let stored: StoredParkedAgentRow = serde_json::from_slice(&bytes).ok()?;
    (row_path(path.parent()?, &stored.agent_id.to_string()) == path).then_some(stored)
}

/// The row path for an agent: `<subagents_dir>/agent-<id>.task.json`, beside
/// its transcript and scoping sidecars.
#[must_use]
pub fn row_path(subagents_dir: &Path, agent_id: &str) -> PathBuf {
    subagents_dir.join(format!("agent-{agent_id}.task.json"))
}

/// Write (or overwrite) the row for a parked agent.
///
/// # Errors
/// Any filesystem error from the directory create or the write.
pub async fn write_row(subagents_dir: &Path, row: &ParkedAgentRow) -> std::io::Result<()> {
    tokio::fs::create_dir_all(subagents_dir).await?;
    let path = row_path(subagents_dir, &row.agent_id.to_string());
    let previous = stored_row_at(&path)
        .await
        .map(|stored| stored.handback_payload)
        .filter(|reference| valid_payload_ref(row.agent_id, reference));
    let origin_session_id = row.request.origin_session_id.or_else(|| {
        row.handback_state
            .as_ref()
            .map(|state| state.run.scope.session_id)
    });
    let payload = ParkedAgentPayload {
        version: 1,
        agent_id: row.agent_id,
        origin_session_id,
        request: row.request.clone(),
        handback_state: row.handback_state.clone(),
        handback_history: row.handback_history.clone(),
    };
    if directory_session(subagents_dir)
        .zip(payload.origin_session_id)
        .is_some_and(|(directory, origin)| directory != origin)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "parked agent payload belongs to another session directory",
        ));
    }
    let bytes = serde_json::to_vec(&payload).map_err(std::io::Error::other)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let reference = ParkedPayloadRef {
        version: 1,
        file: payload_file_name(row.agent_id, &digest),
        sha256: digest,
        origin_session_id,
    };
    let stored = StoredParkedAgentRow {
        task_id: row.task_id.clone(),
        agent_id: row.agent_id,
        description: row.description.clone(),
        handback_opt_in: row.handback_opt_in,
        handback_payload: reference.clone(),
    };
    let metadata = serde_json::to_vec(&stored).map_err(std::io::Error::other)?;
    if metadata.len() as u64 > ROW_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "parked agent metadata exceeds its size limit",
        ));
    }
    // A crash before the row rename leaves its previous complete payload valid.
    atomic_write(&subagents_dir.join(&reference.file), &bytes).await?;
    atomic_write(&path, &metadata).await?;
    if let Some(previous) = previous.filter(|previous| previous.file != reference.file) {
        let _ = tokio::fs::remove_file(subagents_dir.join(previous.file)).await;
    }
    Ok(())
}

/// Remove an agent's row. Missing is success — the caller's intent is "no row
/// afterwards", and a terminal agent may never have parked.
///
/// # Errors
/// Any filesystem error other than "not found".
pub async fn remove_row(subagents_dir: &Path, agent_id: &str) -> std::io::Result<()> {
    let path = row_path(subagents_dir, agent_id);
    let stored = stored_row_at(&path).await;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(stored) = stored {
        let reference = stored.handback_payload;
        if valid_payload_ref(stored.agent_id, &reference) {
            let _ = tokio::fs::remove_file(subagents_dir.join(reference.file)).await;
        }
    }
    Ok(())
}

/// Read one row, if it is present and well-formed.
///
/// Every failure reads as `None`: a row that cannot be parsed describes an
/// agent that cannot be reconstructed, and there is nothing safe to do with a
/// partial launch configuration. Uses `lstat` semantics
/// (`symlink_metadata`) so a symlink planted at the row path is rejected rather
/// than followed — the row decides what gets spawned.
pub async fn read_row(subagents_dir: &Path, agent_id: &str) -> Option<ParkedAgentRow> {
    read_row_at(&row_path(subagents_dir, agent_id)).await
}

async fn read_row_at(path: &Path) -> Option<ParkedAgentRow> {
    let stored = stored_row_at(path).await?;
    let reference = stored.handback_payload;
    if !valid_payload_ref(stored.agent_id, &reference) {
        return None;
    }
    let payload_path = path.parent()?.join(&reference.file);
    if !tokio::fs::symlink_metadata(&payload_path)
        .await
        .ok()?
        .is_file()
    {
        return None;
    }
    let bytes = tokio::fs::read(&payload_path).await.ok()?;
    if format!("{:x}", Sha256::digest(&bytes)) != reference.sha256 {
        return None;
    }
    let payload: ParkedAgentPayload = serde_json::from_slice(&bytes).ok()?;
    if payload.version != 1
        || payload.agent_id != stored.agent_id
        || payload.origin_session_id != reference.origin_session_id
        || payload
            .request
            .origin_session_id
            .is_some_and(|origin| Some(origin) != payload.origin_session_id)
        || payload
            .handback_state
            .iter()
            .chain(&payload.handback_history)
            .any(|state| {
                state.run.agent_id != stored.agent_id
                    || payload
                        .origin_session_id
                        .is_some_and(|origin| state.run.scope.session_id != origin)
            })
    {
        return None;
    }
    if directory_session(path.parent()?)
        .zip(payload.origin_session_id)
        .is_some_and(|(directory, origin)| directory != origin)
    {
        return None;
    }
    Some(ParkedAgentRow {
        task_id: stored.task_id,
        agent_id: stored.agent_id,
        description: stored.description,
        request: payload.request,
        handback_opt_in: stored.handback_opt_in,
        handback_state: payload.handback_state,
        handback_history: payload.handback_history,
    })
}

/// Every restorable row in `subagents_dir`, ordered by agent id so a restore is
/// deterministic.
///
/// A row whose TRANSCRIPT is missing is skipped: the agent's conversation is
/// what a rebuilt runner is seeded from, so a row without one describes an
/// agent that could only be restarted from scratch — which is not a resume,
/// and would silently re-run work the user already paid for.
pub async fn list_restorable(subagents_dir: &Path) -> Vec<ParkedAgentRow> {
    let Ok(mut entries) = tokio::fs::read_dir(subagents_dir).await else {
        return Vec::new();
    };
    let mut out: Vec<ParkedAgentRow> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".task.json"))
        {
            continue;
        }
        let Some(row) = read_row_at(&path).await else {
            continue;
        };
        let transcript =
            crate::forked_skill::agent_transcript_path(subagents_dir, &row.agent_id.to_string());
        if tokio::fs::symlink_metadata(&transcript).await.is_err() {
            continue;
        }
        out.push(row);
    }
    out.sort_by(|a, b| a.agent_id.to_string().cmp(&b.agent_id.to_string()));
    out
}

/// Read a persisted transcript back into the conversation a rebuilt runner is
/// seeded with.
///
/// Malformed lines are SKIPPED rather than aborting the read: a transcript
/// truncated by a crash mid-write would otherwise make the whole agent
/// unresumable, and the last partial line is exactly the one a crash leaves
/// behind.
pub async fn read_transcript_messages(
    subagents_dir: &Path,
    agent_id: &str,
) -> Vec<lingxi_core::types::ConversationMessage> {
    let path = crate::forked_skill::agent_transcript_path(subagents_dir, agent_id);
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("message").cloned())
                .and_then(|m| {
                    serde_json::from_value::<lingxi_core::types::ConversationMessage>(m).ok()
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn request(subagent_type: &str) -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            agent_spawn_token: None,
            stop_hook_scope: Default::default(),
            agent_spawn_provenance: Default::default(),
            teammate_color: None,
            subagent_type: subagent_type.into(),
            prompt: "do the thing".into(),
            observer: None,
            context_paths: Vec::new(),
            description: Some("research".into()),
            model: Some("claude-opus-5".into()),
            model_profile: None,
            run_in_background: true,
            name: Some("rev".into()),
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
        }
    }

    fn row(agent_id: lingxi_core::types::AgentId) -> ParkedAgentRow {
        ParkedAgentRow {
            task_id: "a00000001".into(),
            agent_id,
            description: "research".into(),
            request: request("general-purpose"),
            handback_opt_in: false,
            handback_state: None,
            handback_history: Vec::new(),
        }
    }

    /// Seed a transcript so the row counts as restorable.
    async fn seed_transcript(dir: &Path, agent_id: &str, texts: &[&str]) {
        let path = crate::forked_skill::agent_transcript_path(dir, agent_id);
        let mut body = String::new();
        for t in texts {
            let msg = lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                (*t).to_string(),
            );
            let entry = serde_json::json!({
                "agent_id": agent_id,
                "timestamp": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
                "message": msg,
            });
            body.push_str(&serde_json::to_string(&entry).unwrap());
            body.push('\n');
        }
        tokio::fs::write(path, body).await.unwrap();
    }

    #[tokio::test]
    async fn a_row_round_trips_its_full_launch_configuration() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let r = row(id);
        write_row(dir.path(), &r).await.unwrap();

        let got = read_row(dir.path(), &id.to_string()).await.expect("row");
        assert_eq!(got, r);
        // The pieces a faithful rebuild needs, not just an approximation.
        assert_eq!(got.request.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(got.request.cwd.as_deref(), Some("/repo"));
        assert_eq!(got.request.depth, 1);
        assert_eq!(got.request.name.as_deref(), Some("rev"));
    }

    /// The ABSENCE of a row is the "do not restore" signal — a terminal agent
    /// must never be revived, and relying on absence rather than a status field
    /// means a reader cannot forget to check it.
    #[tokio::test]
    async fn removing_a_row_makes_the_agent_unrestorable() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_row(dir.path(), &row(id)).await.unwrap();
        seed_transcript(dir.path(), &id.to_string(), &["hi"]).await;
        assert_eq!(list_restorable(dir.path()).await.len(), 1);

        remove_row(dir.path(), &id.to_string()).await.unwrap();
        assert!(list_restorable(dir.path()).await.is_empty());
        // Removing again is success — the caller's intent is "no row after".
        remove_row(dir.path(), &id.to_string()).await.unwrap();
    }

    /// A row whose transcript is gone describes an agent that could only be
    /// restarted from SCRATCH, which is not a resume — it would silently re-run
    /// work the user already paid for.
    #[tokio::test]
    async fn a_row_without_a_transcript_is_not_restorable() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_row(dir.path(), &row(id)).await.unwrap();
        assert!(list_restorable(dir.path()).await.is_empty());
    }

    #[tokio::test]
    async fn a_malformed_or_oversized_row_is_ignored() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new().to_string();
        seed_transcript(dir.path(), &id, &["hi"]).await;

        tokio::fs::write(row_path(dir.path(), &id), "{ not json")
            .await
            .unwrap();
        assert!(read_row(dir.path(), &id).await.is_none());
        assert!(list_restorable(dir.path()).await.is_empty());

        tokio::fs::write(
            row_path(dir.path(), &id),
            "x".repeat(ROW_MAX_BYTES as usize + 1),
        )
        .await
        .unwrap();
        assert!(read_row(dir.path(), &id).await.is_none());
    }

    /// `lstat`, not `stat`: the row decides what gets spawned, so a symlink
    /// planted at its path is rejected rather than followed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_row_is_rejected() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let real = dir.path().join("real.json");
        write_row(dir.path(), &row(id)).await.unwrap();
        let metadata = tokio::fs::read(row_path(dir.path(), &id.to_string()))
            .await
            .unwrap();
        tokio::fs::remove_file(row_path(dir.path(), &id.to_string()))
            .await
            .unwrap();
        tokio::fs::write(&real, metadata).await.unwrap();
        std::os::unix::fs::symlink(&real, row_path(dir.path(), &id.to_string())).unwrap();
        assert!(read_row(dir.path(), &id.to_string()).await.is_none());
    }

    #[tokio::test]
    async fn the_transcript_reads_back_as_the_conversation() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new().to_string();
        seed_transcript(dir.path(), &id, &["first", "second"]).await;
        let msgs = read_transcript_messages(dir.path(), &id).await;
        assert_eq!(msgs.len(), 2);
    }

    /// A crash mid-write leaves a truncated LAST line. Skipping it keeps the
    /// agent resumable from everything that did land; aborting the whole read
    /// would make one bad byte cost the entire conversation.
    #[tokio::test]
    async fn a_truncated_final_line_costs_only_that_line() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new().to_string();
        seed_transcript(dir.path(), &id, &["first", "second"]).await;
        let path = crate::forked_skill::agent_transcript_path(dir.path(), &id);
        let mut body = tokio::fs::read_to_string(&path).await.unwrap();
        body.push_str("{\"agent_id\":\"x\",\"mess");
        tokio::fs::write(&path, body).await.unwrap();

        assert_eq!(read_transcript_messages(dir.path(), &id).await.len(), 2);
    }

    #[tokio::test]
    async fn listing_is_deterministic_and_ignores_unrelated_files() {
        let dir = tempdir().unwrap();
        let mut ids: Vec<lingxi_core::types::AgentId> =
            (0..3).map(|_| lingxi_core::types::AgentId::new()).collect();
        for id in &ids {
            write_row(dir.path(), &row(*id)).await.unwrap();
            seed_transcript(dir.path(), &id.to_string(), &["hi"]).await;
        }
        // Neither the transcripts nor a fork sidecar may be read as rows.
        tokio::fs::write(dir.path().join("agent-x.forked-skill.json"), "{}")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("notes.txt"), "x")
            .await
            .unwrap();

        let listed = list_restorable(dir.path()).await;
        assert_eq!(listed.len(), 3);
        ids.sort_by_key(std::string::ToString::to_string);
        assert_eq!(
            listed.iter().map(|r| r.agent_id).collect::<Vec<_>>(),
            ids,
            "ordered by agent id"
        );
    }

    #[tokio::test]
    async fn large_current_report_and_many_archived_reports_round_trip_through_small_metadata() {
        use lingxi_core::host::handback::*;
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let scope = HandbackSessionScope {
            session_id: lingxi_core::types::SessionId::new(),
            activation_epoch: 1,
        };
        let mut source = row(id);
        source.request.origin_session_id = Some(scope.session_id);
        source.request.prompt = "large frozen request".repeat(100_000);
        source.handback_opt_in = true;
        let report = |epoch, size| {
            let run = HandbackRunKey {
                scope,
                agent_id: id,
                run_epoch: epoch,
            };
            let recipient = HandbackRecipient::Main { scope };
            let mut state = HandbackState::new_run(run, true, None, None, recipient, |_| true);
            state.report = Some(HandbackReport {
                text: "r".repeat(size),
                warning: Some(format!("warning {epoch}")),
            });
            state.receipt = Some(HandbackReceipt {
                run,
                recipient,
                message_id: lingxi_core::types::MessageId::new(),
            });
            state.disposition = Some(HandbackDisposition::Flagged);
            state
        };
        source.handback_state = Some(report(20, ROW_MAX_BYTES as usize + 200_000));
        source.handback_history = (1..20).map(|epoch| report(epoch, 100_000)).collect();
        write_row(dir.path(), &source).await.unwrap();
        let metadata = tokio::fs::read(row_path(dir.path(), &id.to_string()))
            .await
            .unwrap();
        assert!(
            metadata.len() < 10_000,
            "row must stay bounded as full reports grow"
        );
        let stored: StoredParkedAgentRow = serde_json::from_slice(&metadata).unwrap();
        let reference = stored.handback_payload;
        let payload_path = dir.path().join(&reference.file);
        assert!(tokio::fs::metadata(&payload_path).await.unwrap().len() > ROW_MAX_BYTES);
        assert_eq!(read_row(dir.path(), &id.to_string()).await.unwrap(), source);
        let old_payload = payload_path;
        source
            .handback_history
            .push(source.handback_state.take().unwrap());
        source.handback_state = Some(report(21, 100_000));
        write_row(dir.path(), &source).await.unwrap();
        assert_eq!(read_row(dir.path(), &id.to_string()).await.unwrap(), source);
        assert!(
            !old_payload.exists(),
            "replaced immutable payload is reclaimed after metadata commits"
        );
        remove_row(dir.path(), &id.to_string()).await.unwrap();
        assert!(tokio::fs::read_dir(dir.path())
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn inline_rows_missing_eligibility_and_mutated_payload_references_are_rejected() {
        let dir = tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let source = row(id);
        let path = row_path(dir.path(), &id.to_string());
        let inline = serde_json::json!({
            "taskId": source.task_id,
            "agentId": source.agent_id,
            "description": source.description,
            "request": source.request,
            "handback_opt_in": source.handback_opt_in,
            "handback_state": null,
            "handback_history": [],
        });
        tokio::fs::write(&path, serde_json::to_vec(&inline).unwrap())
            .await
            .unwrap();
        assert!(read_row(dir.path(), &id.to_string()).await.is_none());
        write_row(dir.path(), &source).await.unwrap();
        let metadata = tokio::fs::read(&path).await.unwrap();
        let mut missing_eligibility: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
        missing_eligibility
            .as_object_mut()
            .unwrap()
            .remove("handback_opt_in");
        tokio::fs::write(&path, serde_json::to_vec(&missing_eligibility).unwrap())
            .await
            .unwrap();
        assert!(read_row(dir.path(), &id.to_string()).await.is_none());
        tokio::fs::write(&path, &metadata).await.unwrap();
        let mut mixed_shape: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
        mixed_shape["request"] = serde_json::to_value(&source.request).unwrap();
        tokio::fs::write(&path, serde_json::to_vec(&mixed_shape).unwrap())
            .await
            .unwrap();
        assert!(read_row(dir.path(), &id.to_string()).await.is_none());
        tokio::fs::write(&path, &metadata).await.unwrap();
        let mut stored: StoredParkedAgentRow = serde_json::from_slice(&metadata).unwrap();
        let reference = &mut stored.handback_payload;
        let payload_path = dir.path().join(&reference.file);
        let bytes = tokio::fs::read(&payload_path).await.unwrap();
        tokio::fs::write(&payload_path, b"{}").await.unwrap();
        assert!(
            read_row(dir.path(), &id.to_string()).await.is_none(),
            "payload hash must verify"
        );
        tokio::fs::write(&payload_path, bytes).await.unwrap();
        reference.file = "../external.json".into();
        tokio::fs::write(&path, serde_json::to_vec(&stored).unwrap())
            .await
            .unwrap();
        assert!(
            read_row(dir.path(), &id.to_string()).await.is_none(),
            "payload path is an exact sibling name"
        );
    }
}
