//! Read-only session journal inspection for independently published branches.

use lingxi_core::host::{
    DurableToolOutput, NativeReceiptRecord, NativeReceiptStage, ToolExecutionRecord,
    ToolExecutionStage, ToolJournalError, ToolJournalRecovery,
};
use lingxi_core::types::SessionId;
use std::collections::HashMap;
use std::path::Path;

/// Read the authoritative tool projection while holding the source writer
/// claim. This never repairs the WAL or rewrites a derivative snapshot.
/// `None` means neither a WAL nor a snapshot exists.
///
/// # Errors
/// Returns an error for a mismatched claim, unreadable or partial WAL, invalid
/// tool transitions, or a snapshot without its authoritative journal.
pub async fn capture_branch_recovery(
    home: &Path,
    source_id: uuid::Uuid,
    lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
) -> Result<Option<ToolJournalRecovery>, String> {
    let session_id = SessionId::from_uuid(source_id);
    if lease.canonical_session_id() != Some(session_id) {
        return Err("branch source writer claim does not match the session".into());
    }
    let directory = home.join("session-state").join(source_id.to_string());
    let wal = tokio::fs::try_exists(directory.join(session::jsonl::JOURNAL_FILE_NAME))
        .await
        .map_err(|error| error.to_string())?;
    let snapshot = tokio::fs::try_exists(directory.join(session::jsonl::SNAPSHOT_FILE_NAME))
        .await
        .map_err(|error| error.to_string())?;
    if !wal {
        return if snapshot {
            Err("session snapshot exists without its authoritative WAL".into())
        } else {
            Ok(None)
        };
    }
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        let relative = std::path::PathBuf::from("session-state").join(source_id.to_string());
        let journal = session::jsonl::DurableJournal::open_under(&home, &relative)
            .map_err(|error| error.to_string())?.with_max_record_bytes(128 * 1024 * 1024);
        let mut projection = ToolProjection::default();
        let mut failure = None;
        let replay = journal
            .replay_read_only_with(|entry| {
                if failure.is_none() {
                    if let Err(error) = fold_branch_entry(&mut projection, session_id, entry) {
                        failure = Some(error);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        if let Some(error) = failure {
            return Err(error);
        }
        if !replay.journal_present {
            return Err("source WAL disappeared during branch recovery inspection".into());
        }
        let mut executions = projection
            .executions
            .into_values()
            .map(ToolExecutionRecord::recovery_view)
            .collect::<Vec<_>>();
        let mut receipts = projection
            .receipts
            .into_values()
            .map(NativeReceiptRecord::recovery_view)
            .collect::<Vec<_>>();
        executions.sort_by_key(ToolExecutionRecord::execution_id);
        receipts.sort_by(|a, b| a.receipt_id.cmp(&b.receipt_id));
        Ok(Some(ToolJournalRecovery {
            executions,
            receipts,
        }))
    })
    .await
    .map_err(|error| error.to_string())?
}

fn fold_branch_entry(
    projection: &mut ToolProjection,
    session_id: SessionId,
    entry: session::jsonl::JournalEntry,
) -> Result<(), String> {
    let serde_json::Value::Object(envelope) = entry.event else {
        return Err("session journal event is not a tagged object".into());
    };
    if envelope.len() != 1 {
        return Err("session journal event must contain exactly one tag".into());
    }
    let (tag, payload) = envelope.into_iter().next().expect("one tag checked");
    match tag.as_str() {
        "ToolExecution" => {
            let record: ToolExecutionRecord =
                serde_json::from_value(payload).map_err(|error| error.to_string())?;
            projection
                .validate_execution(&record, session_id)
                .map_err(|error| error.to_string())?;
            if record.event_id() != entry.event_id {
                return Err("tool journal event identity disagrees with WAL".into());
            }
            projection.executions.insert(record.execution_id(), record);
        }
        "NativeReceipt" => {
            let record: NativeReceiptRecord =
                serde_json::from_value(payload).map_err(|error| error.to_string())?;
            projection
                .validate_receipt(&record, session_id)
                .map_err(|error| error.to_string())?;
            if record.event_id() != entry.event_id {
                return Err("tool journal event identity disagrees with WAL".into());
            }
            projection
                .receipts
                .insert(record.receipt_id.clone(), record);
        }
        "AttemptIntent" => validate_other_event::<cost::AttemptIntent>(payload)?,
        "AttemptReceipt" => {
            validate_other_event::<(cost::AttemptReceipt, cost::CostStateVector)>(payload)?
        }
        "Cost" => validate_other_event::<cost::CostMutationRecord>(payload)?,
        "FusionTerminal" => {
            validate_other_event::<lingxi_core::host::DurableFusionTerminalRecord>(payload)?
        }
        "FusionOutbox" => {
            validate_other_event::<lingxi_core::host::DurableFusionOutboxRecord>(payload)?
        }
        _ => return Err(format!("unknown session journal event tag `{tag}`")),
    }
    Ok(())
}

fn validate_other_event<T: serde::de::DeserializeOwned>(
    payload: serde_json::Value,
) -> Result<(), String> {
    serde_json::from_value::<T>(payload)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolProjection {
    pub(crate) executions: HashMap<String, ToolExecutionRecord>,
    pub(crate) receipts: HashMap<String, NativeReceiptRecord>,
}

pub(crate) fn invalid(message: &str) -> ToolJournalError {
    ToolJournalError(message.into())
}

/// The ledger accepts references, not inline images. Text is retained intact.
pub(crate) fn validate_output(output: &DurableToolOutput) -> Result<(), ToolJournalError> {
    fn contains_image(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(s) => s.starts_with("data:image/"),
            serde_json::Value::Array(v) => v.iter().any(contains_image),
            serde_json::Value::Object(v) => {
                (v.get("type").and_then(|v| v.as_str()) == Some("base64") && v.contains_key("data"))
                    || v.values().any(contains_image)
            }
            _ => false,
        }
    }
    if output.digest.trim().is_empty()
        || output
            .media_refs
            .iter()
            .any(|r| r.trim().is_empty() || r.starts_with("data:"))
        || contains_image(&output.payload)
    {
        return Err(invalid(
            "tool output requires a digest and external media references",
        ));
    }
    Ok(())
}

impl ToolProjection {
    pub(crate) fn pending(&self) -> bool {
        self.executions
            .values()
            .any(|r| !matches!(r.stage, ToolExecutionStage::OutputPublished))
            || self
                .receipts
                .values()
                .any(|r| !matches!(r.stage, NativeReceiptStage::ResponseReceived))
    }

    pub(crate) fn validate_execution(
        &self,
        record: &ToolExecutionRecord,
        session_id: SessionId,
    ) -> Result<(), ToolJournalError> {
        if record.identity.session_id != session_id
            || record.identity.provider_response_id.trim().is_empty()
            || record.identity.provider_call_id.trim().is_empty()
            || record.tool_name.trim().is_empty()
            || record.input_digest.trim().is_empty()
        {
            return Err(invalid("invalid tool execution identity"));
        }
        match record.stage {
            ToolExecutionStage::Started | ToolExecutionStage::OutcomeUnknown => {
                if record.outcome.is_some() || record.output.is_some() {
                    return Err(invalid("unfinished tool execution carries a result"));
                }
            }
            ToolExecutionStage::Terminal => {
                if record.outcome.is_none() || record.output.is_some() {
                    return Err(invalid("terminal requires outcome and no final output"));
                }
            }
            ToolExecutionStage::OutputPrepared | ToolExecutionStage::OutputPublished => {
                if record.outcome.is_none() || record.output.is_none() {
                    return Err(invalid("tool output requires a terminal outcome"));
                }
                validate_output(record.output.as_ref().expect("checked output"))?;
            }
        }
        validate_output(&record.recovery_binding)?;
        let previous = self.executions.get(&record.execution_id());
        let Some(previous) = previous else {
            return if record.stage == ToolExecutionStage::Started {
                Ok(())
            } else {
                Err(invalid("tool execution has no durable Started origin"))
            };
        };
        if previous.identity != record.identity
            || previous.tool_name != record.tool_name
            || previous.input_digest != record.input_digest
            || previous.recovery_binding != record.recovery_binding
            || previous.acknowledged_safety_checks != record.acknowledged_safety_checks
        {
            return Err(invalid("tool execution identity or final input changed"));
        }
        if previous == record {
            return Ok(());
        }
        let allowed = matches!(
            (previous.stage, record.stage),
            (ToolExecutionStage::Started, ToolExecutionStage::Terminal)
                | (
                    ToolExecutionStage::Started,
                    ToolExecutionStage::OutcomeUnknown
                )
                | (
                    ToolExecutionStage::OutcomeUnknown,
                    ToolExecutionStage::OutputPrepared
                )
                | (
                    ToolExecutionStage::Terminal,
                    ToolExecutionStage::OutputPrepared
                )
                | (
                    ToolExecutionStage::OutputPrepared,
                    ToolExecutionStage::OutputPublished
                )
        );
        if previous.stage == ToolExecutionStage::OutcomeUnknown
            && record.outcome != Some(lingxi_core::host::ToolExecutionOutcome::Unknown)
        {
            return Err(invalid("unknown execution cannot acquire a known outcome"));
        }
        if !allowed
            || (previous.outcome.is_some() && previous.outcome != record.outcome)
            || (previous.output.is_some() && previous.output != record.output)
        {
            return Err(invalid(
                "tool execution transition regressed or changed its result",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_receipt(
        &self,
        record: &NativeReceiptRecord,
        session_id: SessionId,
    ) -> Result<(), ToolJournalError> {
        if record.session_id != session_id
            || record.receipt_id.trim().is_empty()
            || record.execution_ids.is_empty()
            || [
                &record.binding.account,
                &record.binding.profile,
                &record.binding.model,
                &record.binding.endpoint,
                &record.binding.protocol,
            ]
            .iter()
            .any(|v| v.trim().is_empty())
        {
            return Err(invalid(
                "invalid native receipt identity or continuation binding",
            ));
        }
        validate_output(&record.receipt)?;
        for execution_id in &record.execution_ids {
            if !self
                .executions
                .get(execution_id)
                .is_some_and(|r| r.stage == ToolExecutionStage::OutputPublished)
            {
                return Err(invalid(
                    "native receipt requires reliably published final tool outputs",
                ));
            }
        }
        match record.stage {
            NativeReceiptStage::ResponseReceived | NativeReceiptStage::ResponsePrepared
                if record
                    .provider_response_id
                    .as_deref()
                    .is_none_or(str::is_empty) =>
            {
                return Err(invalid(
                    "receipt response has no provider response identity",
                ));
            }
            NativeReceiptStage::Prepared
            | NativeReceiptStage::Submitted
            | NativeReceiptStage::NotSubmitted
            | NativeReceiptStage::SubmissionUnknown
            | NativeReceiptStage::CannotResume
                if record.provider_response_id.is_some() =>
            {
                return Err(invalid("unfinished receipt carries a provider response"));
            }
            _ => {}
        }
        if record.stage == NativeReceiptStage::ResponsePrepared && record.response.is_none() {
            return Err(invalid("prepared response requires its complete saved row"));
        }
        if let Some(response) = &record.response {
            validate_output(response)?;
            if !matches!(
                record.stage,
                NativeReceiptStage::ResponsePrepared | NativeReceiptStage::ResponseReceived
            ) {
                return Err(invalid("unfinished receipt carries a saved response"));
            }
        }
        let Some(previous) = self.receipts.get(&record.receipt_id) else {
            return if matches!(
                record.stage,
                NativeReceiptStage::Prepared | NativeReceiptStage::CannotResume
            ) && record.submission_attempt == 0
            {
                Ok(())
            } else {
                Err(invalid("native receipt has no durable preparation"))
            };
        };
        if previous == record {
            return Ok(());
        }
        if previous.session_id != record.session_id
            || previous.execution_ids != record.execution_ids
            || previous.binding != record.binding
            || previous.receipt != record.receipt
            || (previous.response.is_some() && previous.response != record.response)
            || (previous.provider_response_id.is_some()
                && previous.provider_response_id != record.provider_response_id)
        {
            return Err(invalid("native receipt or continuation binding changed"));
        }
        if !matches!(
            (previous.stage, record.stage),
            (NativeReceiptStage::Prepared, NativeReceiptStage::Submitted)
                | (
                    NativeReceiptStage::NotSubmitted,
                    NativeReceiptStage::Submitted
                )
                | (
                    NativeReceiptStage::Submitted,
                    NativeReceiptStage::NotSubmitted
                )
                | (
                    NativeReceiptStage::Submitted,
                    NativeReceiptStage::ResponseReceived
                )
                | (
                    NativeReceiptStage::Submitted,
                    NativeReceiptStage::SubmissionUnknown
                )
                | (
                    NativeReceiptStage::SubmissionUnknown,
                    NativeReceiptStage::ResponseReceived
                )
                | (
                    NativeReceiptStage::Submitted,
                    NativeReceiptStage::ResponsePrepared
                )
                | (
                    NativeReceiptStage::SubmissionUnknown,
                    NativeReceiptStage::ResponsePrepared
                )
                | (
                    NativeReceiptStage::ResponsePrepared,
                    NativeReceiptStage::ResponseReceived
                )
        ) {
            return Err(invalid("native receipt cannot be blindly resubmitted"));
        }
        let expected_attempt = if record.stage == NativeReceiptStage::Submitted {
            previous.submission_attempt.checked_add(1)
        } else {
            Some(previous.submission_attempt)
        };
        if Some(record.submission_attempt) != expected_attempt {
            return Err(invalid(
                "native receipt submission attempt changed illegally",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::live_sessions::SessionWriterLease;
    use lingxi_core::host::{
        NativeContinuationBinding, ToolExecutionIdentity, ToolExecutionOutcome,
    };
    use std::sync::Arc;

    struct Lease(String);
    impl SessionWriterLease for Lease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    #[tokio::test]
    async fn settled_receipt_is_replayed_without_mutating_source_ledger_or_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let relative = std::path::PathBuf::from("session-state").join(id.as_uuid().to_string());
        let journal = session::jsonl::DurableJournal::open_under(home.path(), &relative).unwrap();
        let output = DurableToolOutput {
            digest: "output".into(),
            payload: serde_json::json!({}),
            media_refs: vec![],
        };
        let mut execution = ToolExecutionRecord {
            identity: ToolExecutionIdentity {
                session_id: id,
                provider_response_id: "response".into(),
                provider_call_id: "call".into(),
                member_index: 0,
            },
            tool_name: "computer".into(),
            input_digest: "input".into(),
            recovery_binding: output.clone(),
            acknowledged_safety_checks: vec![],
            stage: ToolExecutionStage::Started,
            outcome: None,
            output: None,
        };
        let append_execution = |record: &ToolExecutionRecord| {
            journal
                .append_once(
                    &record.event_id(),
                    &serde_json::json!({"ToolExecution":record}),
                )
                .unwrap()
        };
        append_execution(&execution);
        execution.stage = ToolExecutionStage::Terminal;
        execution.outcome = Some(ToolExecutionOutcome::Succeeded);
        append_execution(&execution);
        execution.stage = ToolExecutionStage::OutputPrepared;
        execution.output = Some(output.clone());
        append_execution(&execution);
        execution.stage = ToolExecutionStage::OutputPublished;
        append_execution(&execution);
        let mut receipt = NativeReceiptRecord {
            response: None,
            session_id: id,
            receipt_id: "receipt".into(),
            execution_ids: vec![execution.execution_id()],
            binding: NativeContinuationBinding {
                account: "account".into(),
                profile: "profile".into(),
                model: "model".into(),
                endpoint: "endpoint".into(),
                protocol: "responses".into(),
            },
            stage: NativeReceiptStage::Prepared,
            submission_attempt: 0,
            receipt: output,
            provider_response_id: None,
        };
        let append_receipt = |record: &NativeReceiptRecord| {
            journal
                .append_once(
                    &record.event_id(),
                    &serde_json::json!({"NativeReceipt":record}),
                )
                .unwrap()
        };
        append_receipt(&receipt);
        receipt.stage = NativeReceiptStage::Submitted;
        receipt.submission_attempt = 1;
        append_receipt(&receipt);
        receipt.stage = NativeReceiptStage::ResponseReceived;
        receipt.provider_response_id = Some("accepted".into());
        append_receipt(&receipt);
        journal
            .write_snapshot(7, &serde_json::json!({"sentinel":"unchanged"}))
            .unwrap();
        let wal = home
            .path()
            .join(&relative)
            .join(session::jsonl::JOURNAL_FILE_NAME);
        let snapshot = home
            .path()
            .join(&relative)
            .join(session::jsonl::SNAPSHOT_FILE_NAME);
        let before_wal = std::fs::read(&wal).unwrap();
        let before_snapshot = std::fs::read(&snapshot).unwrap();
        let recovery =
            capture_branch_recovery(home.path(), id.as_uuid(), Arc::new(Lease(id.to_string())))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            recovery.executions[0].stage,
            ToolExecutionStage::OutputPublished
        );
        assert_eq!(
            recovery.receipts[0].stage,
            NativeReceiptStage::ResponseReceived
        );
        assert_eq!(std::fs::read(wal).unwrap(), before_wal);
        assert_eq!(std::fs::read(snapshot).unwrap(), before_snapshot);
    }
}
