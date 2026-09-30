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

/// Maximum on-disk size of a row. A row is a few KB; anything approaching this
/// is not a row we wrote, so it is rejected unread.
pub const ROW_MAX_BYTES: u64 = 1_048_576;

/// A parked background agent's launch configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedAgentRow {
    /// The registry task id (`a………`) the agent was known by.
    #[serde(rename = "taskId")]
    pub task_id: String,
    /// The agent id its transcript and sidecars are keyed on.
    #[serde(rename = "agentId")]
    pub agent_id: lingxi_core::types::AgentId,
    /// Human-readable description (the task's).
    pub description: String,
    /// The full spawn request, so the rebuilt runner is configured exactly as
    /// the original was — model, cwd, isolation, depth, tool overrides. This is
    /// the field that makes a restore faithful rather than approximate.
    pub request: SubagentSpawnRequest,
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
    let json = serde_json::to_string(row).map_err(|e| std::io::Error::other(e.to_string()))?;
    tokio::fs::write(row_path(subagents_dir, &row.agent_id.to_string()), json).await
}

/// Remove an agent's row. Missing is success — the caller's intent is "no row
/// afterwards", and a terminal agent may never have parked.
///
/// # Errors
/// Any filesystem error other than "not found".
pub async fn remove_row(subagents_dir: &Path, agent_id: &str) -> std::io::Result<()> {
    match tokio::fs::remove_file(row_path(subagents_dir, agent_id)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > ROW_MAX_BYTES {
        return None;
    }
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str::<ParkedAgentRow>(&text).ok()
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
        }
    }

    fn row(agent_id: lingxi_core::types::AgentId) -> ParkedAgentRow {
        ParkedAgentRow {
            task_id: "a00000001".into(),
            agent_id,
            description: "research".into(),
            request: request("general-purpose"),
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
        tokio::fs::write(&real, serde_json::to_string(&row(id)).unwrap())
            .await
            .unwrap();
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
}
