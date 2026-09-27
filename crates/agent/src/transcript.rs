//! Per-agent transcript writer.
//!
//! [`AgentTranscriptWriter`] appends one JSON line per
//! [`ConversationMessage`] to a transcript file under the agent's transcript
//! subdir.

use platform_api::FileSystem;
use protocol::{AgentId, ConversationMessage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

/// One line in the agent transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Owning agent.
    pub agent_id: AgentId,
    /// Wall-clock timestamp the line was recorded.
    pub timestamp: SystemTime,
    /// The conversation message itself.
    pub message: ConversationMessage,
    /// Terminal agent status for lifecycle records. Ordinary conversation
    /// entries omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Terminal failure detail. Kept out of ordinary message entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional display name of the spawned agent. Older transcripts omit
    /// this field; hosts fall back to the parked task row or agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Resolved agent type (for example `general-purpose`). Persisting this
    /// beside messages lets a live tail expose metadata before a parked task
    /// row exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Concrete provider-local model used by this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider profile paired with [`Self::model`], when pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    /// Caller correlation id (`SubagentContext::correlation_id`, e.g.
    /// Fusion's `{run_id}:p{index}`), when the spawn set one. Lets a
    /// transcript be matched back to the run/panel that produced it (G011).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Appends [`TranscriptEntry`] lines to a per-agent transcript file.
#[derive(Clone)]
pub struct AgentTranscriptWriter {
    /// Absolute path the transcript is written to.
    pub transcript_path: PathBuf,
    /// Owning agent id; stamped on every entry.
    pub agent_id: AgentId,
    /// Sandboxed filesystem used to read/write the transcript.
    fs: Arc<dyn FileSystem>,
    agent_name: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    model_profile: Option<String>,
    correlation_id: Option<String>,
}

impl AgentTranscriptWriter {
    /// Construct a new writer that targets `transcript_path` for `agent_id`.
    #[must_use]
    pub fn new(transcript_path: PathBuf, agent_id: AgentId, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            transcript_path,
            agent_id,
            fs,
            agent_name: None,
            agent_type: None,
            model: None,
            model_profile: None,
            correlation_id: None,
        }
    }

    /// Attach display metadata that is copied onto each entry. This remains a
    /// builder so existing minimal/test callers can keep the old constructor.
    #[must_use]
    pub fn with_metadata(
        mut self,
        agent_name: Option<String>,
        agent_type: Option<String>,
        model: Option<String>,
        model_profile: Option<String>,
    ) -> Self {
        self.agent_name = agent_name;
        self.agent_type = agent_type;
        self.model = model;
        self.model_profile = model_profile;
        self
    }

    /// Attach the caller correlation id (G011) copied onto each entry.
    /// Separate from [`Self::with_metadata`] so existing callers of that
    /// builder are unaffected.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: Option<String>) -> Self {
        self.correlation_id = correlation_id;
        self
    }

    /// Append one [`TranscriptEntry`] for `message`.
    pub async fn record(&self, message: &ConversationMessage) -> Result<(), platform_api::FsError> {
        let entry = TranscriptEntry {
            agent_id: self.agent_id,
            timestamp: SystemTime::now(),
            message: message.clone(),
            status: None,
            error: None,
            agent_name: self.agent_name.clone(),
            agent_type: self.agent_type.clone(),
            model: self.model.clone(),
            model_profile: self.model_profile.clone(),
            correlation_id: self.correlation_id.clone(),
        };
        self.append_entry(&entry).await
    }

    /// Persist explicit historical thinking ranges before the provider retry.
    pub async fn record_thinking_recovery(
        &self,
        messages: std::collections::HashMap<protocol::MessageId, usize>,
    ) -> Result<(), platform_api::FsError> {
        self.record(&ConversationMessage::System {
            id: protocol::MessageId::new(),
            content: serde_json::to_string(&messages).expect("thinking ranges serialize"),
            subtype: Some("thinking_stripped".into()),
            compact_metadata: None,
            refusal_fallback: None,
        })
        .await
    }

    /// Append a terminal lifecycle entry while retaining a normal transcript
    /// message shape for readers that replay only `message` values.
    pub async fn record_terminal(
        &self,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), platform_api::FsError> {
        let detail = error.unwrap_or(status);
        let entry = TranscriptEntry {
            agent_id: self.agent_id,
            timestamp: SystemTime::now(),
            message: ConversationMessage::System {
                id: protocol::MessageId::new(),
                content: detail.to_string(),
                subtype: Some(format!("agent_{status}")),
                compact_metadata: None,
                refusal_fallback: None,
            },
            status: Some(status.to_string()),
            error: error.map(str::to_string),
            agent_name: self.agent_name.clone(),
            agent_type: self.agent_type.clone(),
            model: self.model.clone(),
            model_profile: self.model_profile.clone(),
            correlation_id: self.correlation_id.clone(),
        };
        self.append_entry(&entry).await
    }

    async fn append_entry(&self, entry: &TranscriptEntry) -> Result<(), platform_api::FsError> {
        let line = format!(
            "{}\n",
            serde_json::to_string(entry).expect("transcript serialization")
        );
        let path_str = self
            .transcript_path
            .to_str()
            .expect("utf-8 transcript path");
        // A real APPEND, not read-then-rewrite. The old hack round-tripped the
        // whole file through `read_file`, whose returned view is not
        // guaranteed byte-identical to the file — concatenating onto it
        // corrupted the JSONL — and rewrote every prior line on each message,
        // making a long conversation quadratic.
        self.fs.append_file(path_str, &line).await
    }
}

/// Consume recovery metadata from a restored worker history. The marker is a
/// transcript row, never a provider message; explicit IDs preserve fresh blocks
/// appended after earlier rejections and do not depend on row ordering.
pub(crate) fn restore_thinking_recovery(
    history: &mut Vec<ConversationMessage>,
    scope: &llm_runtime::thinking_scope::ThinkingRecoveryScope,
) {
    history.retain(|message| {
        if let ConversationMessage::System {
            content,
            subtype: Some(subtype),
            ..
        } = message
        {
            if subtype == "thinking_stripped" {
                if let Ok(ranges) = serde_json::from_str(content) {
                    scope.merge(ranges);
                }
                return false;
            }
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worker_thinking_recovery_round_trips_ranges_and_preserves_fresh_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-recovery.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        );
        let assistant = |id, text: &str| ConversationMessage::Assistant {
            id,
            content: vec![
                protocol::ContentBlock::Thinking {
                    thinking: "keep-prefix".into(),
                    signature: Some("valid".into()),
                },
                protocol::ContentBlock::Thinking {
                    thinking: text.into(),
                    signature: Some("sig".into()),
                },
                protocol::ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            stop_reason: Some("end_turn".into()),
        };
        let old = assistant(protocol::MessageId::new(), "rejected");
        let fresh = assistant(protocol::MessageId::new(), "fresh");
        writer.record(&old).await.unwrap();
        writer
            .record_thinking_recovery([(old.id(), 1)].into_iter().collect())
            .await
            .unwrap();
        writer.record(&fresh).await.unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        let mut history: Vec<_> = body
            .lines()
            .map(|line| {
                serde_json::from_str::<TranscriptEntry>(line)
                    .unwrap()
                    .message
            })
            .collect();
        let scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
        restore_thinking_recovery(&mut history, &scope);
        assert_eq!(history.len(), 2);
        assert_eq!(scope.messages().get(&old.id()), Some(&1));
        assert!(!scope.messages().contains_key(&fresh.id()));
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut history,
            &scope.messages(),
        );
        let ConversationMessage::Assistant { content, .. } = &history[0] else {
            panic!("assistant")
        };
        assert!(
            matches!(&content[0], protocol::ContentBlock::Thinking { thinking, .. } if thinking == "keep-prefix")
        );
        assert_eq!(content.len(), 2);
        assert_eq!(history[1], fresh);
    }
}
