//! Durable host boundary for side effects and provider receipts.
//!
//! A Started ACK authorizes input only when `duplicate` is false. Recovery never
//! authorizes replay; callers publish an existing output or ask for observation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::SessionId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionIdentity {
    pub session_id: SessionId,
    pub provider_response_id: String,
    pub provider_call_id: String,
    pub member_index: u32,
}

impl ToolExecutionIdentity {
    pub fn call_identity(&self) -> (String, String) {
        (
            self.provider_response_id.clone(),
            self.provider_call_id.clone(),
        )
    }
    /// Length framed identities avoid collisions between response/call strings.
    pub fn execution_id(&self) -> String {
        let mut digest = Sha256::new();
        for value in [
            self.session_id.to_string(),
            self.provider_response_id.clone(),
            self.provider_call_id.clone(),
            self.member_index.to_string(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        format!("tool-execution:{:x}", digest.finalize())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionOutcome {
    /// Input may have occurred; this outcome must never authorize replay.
    Unknown,
    Succeeded,
    Failed,
    Denied,
    Cancelled,
    Skipped,
}

/// Final model-visible result after hooks, with image bytes externalized by the
/// host's existing media store. Payload contains media references, never data URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableToolOutput {
    pub digest: String,
    pub payload: serde_json::Value,
    pub media_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionStage {
    Started,
    Terminal,
    OutputPrepared,
    OutputPublished,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionRecord {
    pub identity: ToolExecutionIdentity,
    pub tool_name: String,
    pub input_digest: String,
    /// Immutable call/route recovery data, independent of compactable history.
    pub recovery_binding: DurableToolOutput,
    /// Safety decisions actually confirmed before this input was admitted.
    pub acknowledged_safety_checks: Vec<serde_json::Value>,
    pub stage: ToolExecutionStage,
    pub outcome: Option<ToolExecutionOutcome>,
    pub output: Option<DurableToolOutput>,
}

impl ToolExecutionRecord {
    pub fn execution_id(&self) -> String {
        self.identity.execution_id()
    }

    pub fn event_id(&self) -> String {
        format!("{}:{:?}", self.execution_id(), self.stage)
    }

    /// An interrupted side effect must be observed, never repeated.
    pub fn recovery_view(mut self) -> Self {
        if self.stage == ToolExecutionStage::Started {
            self.stage = ToolExecutionStage::OutcomeUnknown;
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeContinuationBinding {
    pub account: String,
    pub profile: String,
    pub model: String,
    pub endpoint: String,
    pub protocol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeReceiptStage {
    /// Final hook-visible output cannot form a legal provider continuation.
    /// Retained as a terminal blocker; no hidden image or request may replace it.
    CannotResume,
    Prepared,
    Submitted,
    /// Dispatch admission was revoked before any transport send occurred.
    NotSubmitted,
    /// A complete validated successor is saved; ordinary row publication can be repaired.
    ResponsePrepared,
    ResponseReceived,
    SubmissionUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReceiptRecord {
    pub session_id: SessionId,
    pub receipt_id: String,
    pub execution_ids: Vec<String>,
    pub binding: NativeContinuationBinding,
    pub stage: NativeReceiptStage,
    /// Each admitted submission has a distinct WAL identity, including safe retries.
    pub submission_attempt: u32,
    /// Exact immutable receipt or CannotResume error facts, with external media references.
    pub receipt: DurableToolOutput,
    /// Complete accepted successor row saved before publication/Received acknowledgement.
    pub response: Option<DurableToolOutput>,
    pub provider_response_id: Option<String>,
}

impl NativeReceiptRecord {
    pub fn event_id(&self) -> String {
        format!(
            "native-receipt:{}:{}:{:?}",
            self.receipt_id, self.submission_attempt, self.stage
        )
    }

    pub fn recovery_view(mut self) -> Self {
        if self.stage == NativeReceiptStage::Submitted {
            self.stage = NativeReceiptStage::SubmissionUnknown;
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolJournalAck {
    pub event_id: String,
    pub journal_revision: u64,
    /// A duplicate Started ACK must never authorize another side effect.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("tool execution journal: {0}")]
pub struct ToolJournalError(pub String);

/// Authoritative durable records for one canonical session. Started/submitted
/// records appear with unknown recovery stages and never authorize replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolJournalRecovery {
    pub executions: Vec<ToolExecutionRecord>,
    pub receipts: Vec<NativeReceiptRecord>,
}

#[async_trait]
pub trait ToolExecutionJournal: Send + Sync {
    /// Enumerate startup recovery views independently of transcript markers.
    /// Call only when reconstructing an owner after excluding live execution;
    /// unfinished records appear Unknown without rewriting a live projection.
    /// Implementations without recovery must reject, rather than imply empty state.
    async fn recover_session(
        &self,
        _session_id: SessionId,
    ) -> Result<ToolJournalRecovery, ToolJournalError> {
        Err(ToolJournalError("recovery query unavailable".into()))
    }

    async fn record_execution(
        &self,
        record: ToolExecutionRecord,
    ) -> Result<ToolJournalAck, ToolJournalError>;
    /// Return the actual latest persisted state, including live Started.
    /// Unknown stages are persisted during fresh coordinator startup recovery.
    async fn execution(
        &self,
        execution_id: &str,
    ) -> Result<Option<ToolExecutionRecord>, ToolJournalError>;
    async fn record_receipt(
        &self,
        record: NativeReceiptRecord,
    ) -> Result<ToolJournalAck, ToolJournalError>;
    /// Return the actual latest persisted receipt state, including live Submitted.
    async fn receipt(
        &self,
        receipt_id: &str,
    ) -> Result<Option<NativeReceiptRecord>, ToolJournalError>;
}
