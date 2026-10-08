//! Recipient-owned admission and consumption of ordinary SubagentHandback reports.
//!
//! Admission is independent of the main turn gate. The private sidecar is the
//! durable queue; the only transcript record is the eventual native peer user
//! message. A stable prepared row makes retry after an uncertain fsync safe.

use super::*;
use lingxi_core::host::handback::{
    HandbackAdmissionError, HandbackEnvelope, HandbackRecipient, HandbackSessionScope,
    ReportingAdmission,
};
use lingxi_core::host::orchestrator::MainReportWaker;
use serde::{Deserialize, Serialize};
use session::jsonl::exact_json::Utf16Overrides;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

const INBOX_VERSION: u32 = 1;
const MAX_INBOX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PendingReport {
    envelope: HandbackEnvelope,
    #[serde(default)]
    prepared_row: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Utf16Overrides::is_empty")]
    prepared_utf16_overrides: Utf16Overrides,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedInbox {
    version: u32,
    scope: HandbackSessionScope,
    pending: VecDeque<PendingReport>,
    admitted: HashMap<MessageId, String>,
}

pub(crate) struct MainReportState {
    stored: PersistedInbox,
    recovered: bool,
    closed: bool,
}

pub(crate) struct MainReportInbox {
    pub(crate) state: Mutex<MainReportState>,
    waker: std::sync::OnceLock<Arc<dyn MainReportWaker>>,
    enabled: std::sync::atomic::AtomicBool,
    /// A cancelled prepare may drop its queue guard while a blocking atomic
    /// write is still running. The blocking owner retains this gate so that
    /// stale sidecar I/O cannot overwrite a subsequent admitted snapshot.
    store_gate: Arc<Mutex<()>>,
}

impl MainReportInbox {
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            state: Mutex::new(MainReportState::new(session_id, 1)),
            waker: std::sync::OnceLock::new(),
            enabled: std::sync::atomic::AtomicBool::new(false),
            store_gate: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn reset_for_builder(&self, session_id: SessionId) {
        *self
            .state
            .try_lock()
            .expect("report inbox builder is exclusive") = MainReportState::new(session_id, 1);
    }
}

impl MainReportState {
    fn new(session_id: SessionId, activation_epoch: u64) -> Self {
        Self {
            stored: PersistedInbox {
                version: INBOX_VERSION,
                scope: HandbackSessionScope {
                    session_id,
                    activation_epoch,
                },
                pending: VecDeque::new(),
                admitted: HashMap::new(),
            },
            recovered: false,
            closed: false,
        }
    }

    /// Called in the same critical section that publishes the session state.
    pub(crate) fn activate(&mut self, session_id: SessionId) {
        let Some(next_epoch) = self.stored.scope.activation_epoch.checked_add(1) else {
            // Never reuse an exhausted epoch as a valid receiving capability.
            self.closed = true;
            return;
        };
        let closed = self.closed;
        *self = Self::new(session_id, next_epoch);
        self.closed = closed;
    }
}

fn rejected(reason: impl Into<String>) -> HandbackAdmissionError {
    HandbackAdmissionError::Rejected {
        reason: reason.into(),
    }
}

fn envelope_digest(envelope: &HandbackEnvelope) -> Result<String, HandbackAdmissionError> {
    let bytes = serde_json::to_vec(envelope).map_err(|error| rejected(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_prepared_report_row(
    envelope: &HandbackEnvelope,
    row: &serde_json::Value,
    utf16_overrides: &Utf16Overrides,
) -> Result<(), HandbackAdmissionError> {
    let target = envelope.receipt.recipient.scope().session_id;
    let valid = row.get("type").and_then(serde_json::Value::as_str) == Some("user")
        && row.get("uuid").and_then(serde_json::Value::as_str)
            == Some(envelope.receipt.message_id.as_uuid().to_string().as_str())
        && row
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .and_then(SessionId::parse_prefixed)
            == Some(target)
        && row.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true)
        && row.get("origin") == Some(&native_peer_origin(envelope))
        && row.get("queuePriority").is_none()
        && utf16_overrides == &native_report_utf16_overrides(envelope)
        && row
            .get("isModelContextExcluded")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        && row
            .get("isVisibleInTranscriptOnly")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        && row.get("message") == Some(&native_report_message(envelope));
    if valid {
        Ok(())
    } else {
        Err(rejected(
            "prepared report row lost its identity or Peer authority",
        ))
    }
}

/// The transcript keeps native text blocks. Exact JavaScript string units are
/// carried only in the private prepared claim and in the JSON string encoder.
fn native_report_message(envelope: &HandbackEnvelope) -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": [{"type": "text", "text": envelope.model_message_text()}],
    })
}

fn native_report_utf16_overrides(envelope: &HandbackEnvelope) -> Utf16Overrides {
    let Some(units) = &envelope.body_utf16 else {
        return Utf16Overrides::new();
    };
    Utf16Overrides::from([
        (
            "/message/content/0/text".into(),
            lingxi_core::host::handback_wire::render_agent_message_utf16(
                &envelope.origin.from,
                units,
            ),
        ),
        (
            "/origin/body".into(),
            lingxi_core::host::handback_wire::neutralize_peer_frames_utf16(units),
        ),
    ])
}

fn native_peer_origin(envelope: &HandbackEnvelope) -> serde_json::Value {
    let body = envelope.body_utf16.as_ref().map_or_else(
        || lingxi_core::host::handback_wire::neutralize_peer_frames(&envelope.body),
        |units| {
            String::from_utf16_lossy(
                &lingxi_core::host::handback_wire::neutralize_peer_frames_utf16(units),
            )
        },
    );
    let mut origin = serde_json::json!({
        "kind": "peer",
        "from": envelope.origin.from,
        "senderTaskId": envelope.origin.sender_task_id,
        "body": body,
        "handback": true,
    });
    if let Some(name) = &envelope.origin.name {
        if !name.is_empty() {
            origin["name"] = serde_json::Value::String(name.clone());
        }
    }
    if envelope.origin.flagged {
        origin["flagged"] = serde_json::Value::Bool(true);
    }
    origin
}

impl ConversationOrchestrator {
    /// Install the host's normal turn scheduling. Report bodies never pass
    /// through human prompt batching, slash expansion, or attachment discovery.
    pub fn set_main_report_waker(&self, waker: Arc<dyn MainReportWaker>) {
        self.main_reports
            .enabled
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.main_reports.waker.set(waker);
    }

    fn main_report_store(&self, scope: HandbackSessionScope) -> Option<(PathBuf, PathBuf)> {
        if let Some(home) = &self.config_home {
            return Some((
                home.clone(),
                PathBuf::from("sessions")
                    .join("handback-inboxes")
                    .join(format!("{}.json", scope.session_id.as_uuid())),
            ));
        }
        let writer = self.transcript.jsonl_writer.as_ref()?;
        let path = writer.session_target_path(scope.session_id)?;
        Some((
            path.parent()?.to_path_buf(),
            PathBuf::from(format!(
                "{}.handback-inbox.json",
                scope.session_id.as_uuid()
            )),
        ))
    }

    async fn save_main_report_inbox(
        &self,
        stored: &PersistedInbox,
    ) -> Result<(), HandbackAdmissionError> {
        if self
            .transcript
            .jsonl_writer
            .as_ref()
            .is_some_and(|writer| !writer.durable_transcript_enabled())
        {
            return Err(HandbackAdmissionError::Unavailable);
        }
        let Some((root, relative)) = self.main_report_store(stored.scope) else {
            // Value-based, in-memory embedders intentionally own a volatile queue.
            // A host with a transcript must provide a pinned durable authority.
            return if self.transcript.jsonl_writer.is_none() {
                Ok(())
            } else {
                Err(HandbackAdmissionError::Unavailable)
            };
        };
        let bytes = serde_json::to_vec(stored).map_err(|error| rejected(error.to_string()))?;
        if bytes.len() as u64 > MAX_INBOX_BYTES {
            return Err(rejected("main report inbox exceeds its storage limit"));
        }
        let store_guard = self.main_reports.store_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _store_guard = store_guard;
            lingxi_core::host::rooted_fs::atomic_write(
                &root,
                &relative,
                &bytes,
                lingxi_core::host::AtomicWriteOptions::default(),
            )
        })
        .await
        .map_err(|error| rejected(error.to_string()))?
        .map_err(|error| rejected(error.to_string()))
    }

    async fn recover_main_report_inbox_locked(
        &self,
        state: &mut MainReportState,
    ) -> Result<(), HandbackAdmissionError> {
        if state.recovered {
            return Ok(());
        }
        let current = state.stored.scope;
        let recovered = if let Some((root, relative)) = self.main_report_store(current) {
            let store_guard = self.main_reports.store_gate.clone().lock_owned().await;
            let read = tokio::task::spawn_blocking(move || {
                let _store_guard = store_guard;
                lingxi_core::host::rooted_fs::read_to_string_limited(
                    &root,
                    &relative,
                    MAX_INBOX_BYTES,
                )
            })
            .await
            .map_err(|error| rejected(error.to_string()))?;
            match read {
                Ok(raw) => Some(
                    serde_json::from_str::<PersistedInbox>(&raw)
                        .map_err(|error| rejected(format!("invalid main report inbox: {error}")))?,
                ),
                Err(lingxi_core::host::FsError::NotFound(_)) => None,
                Err(error) => return Err(rejected(error.to_string())),
            }
        } else {
            None
        };
        let mut candidate = state.stored.clone();
        if let Some(stored) = recovered {
            if stored.version != INBOX_VERSION || stored.scope.session_id != current.session_id {
                return Err(rejected(
                    "main report inbox belongs to another session or version",
                ));
            }
            candidate.scope.activation_epoch = current.activation_epoch.max(
                stored
                    .scope
                    .activation_epoch
                    .checked_add(1)
                    .ok_or_else(|| rejected("main report activation epoch exhausted"))?,
            );
            // Bind the private queue's wake owner to this activation of the
            // SAME session. The admitted receipt and Peer provenance remain
            // immutable, including their original activation epoch.
            let mut pending_ids = HashSet::new();
            for pending in &stored.pending {
                match pending.envelope.receipt.recipient {
                    HandbackRecipient::Main { scope } if scope.session_id == current.session_id => {
                    }
                    _ => return Err(rejected("main report inbox contains another recipient")),
                }
                let message_id = pending.envelope.receipt.message_id;
                if !pending.envelope.validate()
                    || !pending_ids.insert(message_id)
                    || stored.admitted.get(&message_id)
                        != Some(&envelope_digest(&pending.envelope)?)
                {
                    return Err(rejected(
                        "main report inbox contains inconsistent admission or Peer provenance",
                    ));
                }
                if let Some(row) = &pending.prepared_row {
                    validate_prepared_report_row(
                        &pending.envelope,
                        row,
                        &pending.prepared_utf16_overrides,
                    )?;
                } else if !pending.prepared_utf16_overrides.is_empty() {
                    return Err(rejected(
                        "unprepared report contains transcript string overrides",
                    ));
                }
            }
            candidate.pending = stored.pending;
            candidate.admitted = stored.admitted;
        }
        self.save_main_report_inbox(&candidate).await?;
        state.stored = candidate;
        state.recovered = true;
        Ok(())
    }

    /// Recover this recipient's queue and announce any pending peer work.
    /// Hosts call after installing the same admission object and wake owner.
    pub async fn recover_main_reports(&self) -> Result<(), HandbackAdmissionError> {
        self.main_reports
            .enabled
            .store(true, std::sync::atomic::Ordering::Release);
        let wakes = {
            let mut state = self.main_reports.state.lock().await;
            if state.closed {
                return Err(HandbackAdmissionError::Unavailable);
            }
            self.recover_main_report_inbox_locked(&mut state).await?;
            state
                .stored
                .pending
                .iter()
                .map(|item| (state.stored.scope, item.envelope.receipt.message_id))
                .collect::<Vec<_>>()
        };
        if let Some(waker) = self.main_reports.waker.get() {
            for (scope, message_id) in wakes {
                waker.wake(scope, message_id).await;
            }
        }
        Ok(())
    }

    /// A previously queued wake belongs to the old activation. Recover and
    /// announce this session's accepted reports after a hot activation too,
    /// under an owned lifecycle claim. The transition never waits for a model
    /// turn while it still owns `turn_gate`.
    pub(crate) fn recover_main_reports_after_activation(&self) {
        if !self
            .main_reports
            .enabled
            .load(std::sync::atomic::Ordering::Acquire)
            || self.main_reports.waker.get().is_none()
        {
            return;
        }
        let Some(owner) = self
            .lifecycle_runtime
            .session_switch_owner
            .get()
            .and_then(std::sync::Weak::upgrade)
        else {
            return;
        };
        let Ok(claim) = self.lifecycle_runtime.session_switch_supervisor.claim() else {
            return;
        };
        tokio::spawn(async move {
            let error = match owner.recover_main_reports().await {
                Ok(()) | Err(HandbackAdmissionError::Unavailable) => None,
                Err(error) => {
                    tracing::warn!(%error, "main report recovery after session activation failed");
                    Some(format!(
                        "main report recovery after session activation failed: {error}"
                    ))
                }
            };
            claim.complete(error);
        });
    }

    pub(crate) async fn close_main_report_admission(&self) {
        self.main_reports.state.lock().await.closed = true;
    }

    /// Recheck queued peer work under the turn owner's gate before waking.
    pub async fn has_pending_main_reports(&self, scope: HandbackSessionScope) -> bool {
        let state = self.main_reports.state.lock().await;
        !state.closed && state.stored.scope == scope && !state.stored.pending.is_empty()
    }

    async fn admit_main_report_owned(
        &self,
        envelope: HandbackEnvelope,
    ) -> Result<(), HandbackAdmissionError> {
        let message_id = envelope.receipt.message_id;
        let scope = {
            let mut state = self.main_reports.state.lock().await;
            if state.closed {
                return Err(HandbackAdmissionError::Unavailable);
            }
            self.recover_main_report_inbox_locked(&mut state).await?;
            let scope = state.stored.scope;
            if !matches!(&envelope.receipt.recipient, HandbackRecipient::Main { scope: target } if *target == scope)
                || envelope.origin.scope != envelope.receipt.run.scope
                || envelope.origin.sender_agent_id != envelope.receipt.run.agent_id
                || envelope.receipt.run.scope != scope
            {
                return Err(HandbackAdmissionError::StaleScope);
            }
            if !envelope.validate() {
                return Err(rejected(
                    "report envelope has invalid sender identity or UTF-16 body",
                ));
            }
            let digest = envelope_digest(&envelope)?;
            if let Some(prior) = state.stored.admitted.get(&message_id) {
                return if prior == &digest {
                    Ok(())
                } else {
                    Err(rejected(
                        "report MessageId conflicts with an admitted report",
                    ))
                };
            }
            let mut candidate = state.stored.clone();
            candidate.admitted.insert(message_id, digest);
            candidate.pending.push_back(PendingReport {
                envelope,
                prepared_row: None,
                prepared_utf16_overrides: Utf16Overrides::new(),
            });
            self.save_main_report_inbox(&candidate).await?;
            state.stored = candidate;
            scope
        };
        // Waking is a separate owned operation: admission never waits for the
        // parent's model turn, UI owner, or turn gate.
        if let Some(waker) = self.main_reports.waker.get().cloned() {
            tokio::spawn(async move {
                waker.wake(scope, message_id).await;
            });
        }
        Ok(())
    }

    /// Called at serialized ordinary main preparation. Each item remains
    /// pending until both its stable transcript row and live history commit.
    pub(crate) async fn consume_main_reports(&self) -> Result<bool, OrchestratorError> {
        if !self
            .main_reports
            .enabled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(false);
        }
        if self
            .transcript
            .jsonl_writer
            .as_ref()
            .is_some_and(|writer| !writer.durable_transcript_enabled())
        {
            // Such a recipient cannot advertise reporting admission. Its
            // ordinary turns still run without a report queue.
            return Ok(false);
        }
        let mut state = self.main_reports.state.lock().await;
        if state.closed {
            return Ok(false);
        }
        self.recover_main_report_inbox_locked(&mut state)
            .await
            .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
        let target = state.stored.scope;
        if self.session.lock().await.session_id != target.session_id {
            return Err(OrchestratorError::Internal(
                "main report activation differs from live session".into(),
            ));
        }
        let mut consumed = false;
        while let Some(pending) = state.stored.pending.front().cloned() {
            let message_id = pending.envelope.receipt.message_id;
            let message = pending.envelope.model_message();
            if pending.prepared_row.is_none() {
                let mut candidate = state.stored.clone();
                if let Some(writer) = &self.transcript.jsonl_writer {
                    if !writer.durable_transcript_enabled() {
                        return Err(OrchestratorError::Internal(
                            "peer report consumption requires durable transcript authority".into(),
                        ));
                    }
                    let mut row = self.to_jsonl_message_with_inner_id(
                        &message,
                        &target.session_id.as_uuid().to_string(),
                        None,
                        self.resolve_git_branch().await,
                        Some(entrypoint_value()),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    row.cwd = writer
                        .session_target_cwd(target.session_id)
                        .unwrap_or_else(|| self.current_cwd())
                        .to_string_lossy()
                        .into_owned();
                    row.message = native_report_message(&pending.envelope);
                    let origin = native_peer_origin(&pending.envelope);
                    row.extra.insert("origin".into(), origin);
                    let prepared = candidate.pending.front_mut().expect("pending front");
                    prepared.prepared_row = Some(
                        serde_json::to_value(row)
                            .map_err(|error| OrchestratorError::Internal(error.to_string()))?,
                    );
                    prepared.prepared_utf16_overrides =
                        native_report_utf16_overrides(&pending.envelope);
                }
                self.save_main_report_inbox(&candidate)
                    .await
                    .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
                state.stored = candidate;
            }
            if let Some(writer) = &self.transcript.jsonl_writer {
                let prepared = state.stored.pending.front().expect("pending front");
                let row = prepared
                    .prepared_row
                    .clone()
                    .expect("durable prepared report row");
                let uuid = message_id.as_uuid().to_string();
                let result = writer
                    .append_json_once_durable_for_session_with_tip_exact(
                        target.session_id,
                        &format!("subagent-handback:{uuid}"),
                        row,
                        prepared.prepared_utf16_overrides.clone(),
                    )
                    .await;
                self.reconcile_fusion_transcript_append(target.session_id, uuid, result)
                    .await
                    .map_err(|error| {
                        OrchestratorError::Internal(format!(
                            "main report persistence failed: {error}"
                        ))
                    })?;
            }
            {
                let mut session = self.session.lock().await;
                if !session.history.iter().any(|item| item.id() == message_id) {
                    session.history.push(message);
                }
            }
            let mut acknowledged = state.stored.clone();
            acknowledged.pending.pop_front();
            self.save_main_report_inbox(&acknowledged)
                .await
                .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
            state.stored = acknowledged;
            consumed = true;
        }
        Ok(consumed)
    }
}

#[async_trait]
impl ReportingAdmission for ConversationOrchestrator {
    async fn main_scope(&self) -> Option<HandbackSessionScope> {
        self.main_reports
            .enabled
            .store(true, std::sync::atomic::Ordering::Release);
        let mut state = self.main_reports.state.lock().await;
        if state.closed {
            return None;
        }
        if let Err(error) = self.recover_main_report_inbox_locked(&mut state).await {
            tracing::warn!(%error, "main report admission recovery failed");
            return None;
        }
        Some(state.stored.scope)
    }

    async fn admit(&self, envelope: HandbackEnvelope) -> Result<(), HandbackAdmissionError> {
        let owner = self
            .lifecycle_runtime
            .session_switch_owner
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or(HandbackAdmissionError::Unavailable)?;
        let claim = self
            .lifecycle_runtime
            .session_switch_supervisor
            .claim()
            .map_err(|_| HandbackAdmissionError::Unavailable)?;
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = owner.admit_main_report_owned(envelope).await;
            match result_tx.send(result) {
                Ok(()) => claim.complete(None),
                Err(result) => claim.complete(
                    result
                        .err()
                        .map(|error| format!("detached main report admission failed: {error}")),
                ),
            }
        });
        result_rx
            .await
            .map_err(|_| HandbackAdmissionError::Unavailable)?
    }
}

#[cfg(test)]
#[path = "tests/main_report_admission_tests.rs"]
mod tests;
