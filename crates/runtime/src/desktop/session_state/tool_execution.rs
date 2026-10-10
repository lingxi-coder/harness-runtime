//! Tool side effects use the canonical session writer, queue and retention pin.
use super::*;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub(super) struct ToolProjection {
    pub(super) executions: HashMap<String, ToolExecutionRecord>,
    pub(super) receipts: HashMap<String, NativeReceiptRecord>,
}

fn invalid(message: &str) -> ToolJournalError {
    ToolJournalError(message.into())
}

/// The ledger accepts references, not inline images. Text is retained intact.
fn validate_output(output: &DurableToolOutput) -> Result<(), ToolJournalError> {
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
    pub(super) fn pending(&self) -> bool {
        self.executions
            .values()
            .any(|r| !matches!(r.stage, ToolExecutionStage::OutputPublished))
            || self
                .receipts
                .values()
                .any(|r| !matches!(r.stage, NativeReceiptStage::ResponseReceived))
    }

    fn validate_execution(
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

    fn validate_receipt(
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

    fn validate(
        &self,
        event: &SessionEvent,
        session_id: SessionId,
    ) -> Result<String, ToolJournalError> {
        match event {
            SessionEvent::ToolExecution(record) => {
                self.validate_execution(record, session_id)?;
                Ok(record.event_id())
            }
            SessionEvent::NativeReceipt(record) => {
                self.validate_receipt(record, session_id)?;
                Ok(record.event_id())
            }
            _ => Err(invalid("not a tool journal event")),
        }
    }

    pub(super) fn fold(
        &mut self,
        event: &SessionEvent,
        session_id: SessionId,
        event_id: &str,
    ) -> Result<(), ToolJournalError> {
        if self.validate(event, session_id)? != event_id {
            return Err(invalid("tool journal event identity disagrees with WAL"));
        }
        match event {
            SessionEvent::ToolExecution(record) => {
                self.executions
                    .insert(record.execution_id(), record.clone());
            }
            SessionEvent::NativeReceipt(record) => {
                self.receipts
                    .insert(record.receipt_id.clone(), record.clone());
            }
            _ => unreachable!("validated tool event"),
        }
        Ok(())
    }
}

impl CoordinatorState {
    pub(super) fn persist_tool_event(
        &self,
        event: SessionEvent,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.persist_tool_event_locked(event)
    }

    fn persist_tool_event_locked(
        &self,
        event: SessionEvent,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        let event_id = match &event {
            SessionEvent::ToolExecution(r) => r.event_id(),
            SessionEvent::NativeReceipt(r) => r.event_id(),
            _ => return Err(invalid("not a tool journal event")),
        };
        let value = encode_session_event(&event).map_err(|e| ToolJournalError(e.to_string()))?;
        // Compare exact durable payload before validating current state, so an
        // ACK retry for Started after Terminal remains idempotent but grants no input.
        if let Some(existing) = self
            .journal
            .find_event_durable(&event_id)
            .map_err(|e| ToolJournalError(e.to_string()))?
        {
            if existing.event != value {
                return Err(invalid("conflicting durable tool event"));
            }
            return Ok(ToolJournalAck {
                event_id,
                journal_revision: existing.journal_revision,
                duplicate: true,
            });
        }
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(ToolJournalError(reason));
        }
        self.projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .validate(&event, self.session_id)?;
        let append = self
            .journal
            .append_once(&event_id, &value)
            .map_err(|e| ToolJournalError(e.to_string()))?;
        self.projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .fold(&event, self.session_id, &event_id)?;
        self.note_durable_append(append.journal_revision);
        Ok(ToolJournalAck {
            event_id,
            journal_revision: append.journal_revision,
            duplicate: append.duplicate,
        })
    }
    /// Called only at startup under mutation_gate; uncertainty is absorbing for input.
    pub(super) fn recover_tool_events(&self) -> Result<(), CostPersistError> {
        let pending = {
            let projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut events = projection
                .tools
                .executions
                .values()
                .filter(|r| r.stage == ToolExecutionStage::Started)
                .cloned()
                .map(|r| SessionEvent::ToolExecution(r.recovery_view()))
                .collect::<Vec<_>>();
            events.extend(
                projection
                    .tools
                    .receipts
                    .values()
                    .filter(|r| r.stage == NativeReceiptStage::Submitted)
                    .cloned()
                    .map(|r| SessionEvent::NativeReceipt(r.recovery_view())),
            );
            events
        };
        for event in pending {
            let ack = self
                .persist_tool_event_locked(event)
                .map_err(|e| CostPersistError::Storage(e.0))?;
            if let Some((_, revision)) = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .latest
                .as_mut()
            {
                *revision = ack.journal_revision;
            }
        }
        Ok(())
    }
}

impl SessionStateCoordinator {
    pub(super) fn tool_projection_pending(&self) -> bool {
        self.state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .pending()
    }

    async fn append_tool_event(
        &self,
        event: SessionEvent,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        let (permit, pin) = self
            .acquire_session_mutation_permit()
            .await
            .map_err(|e| ToolJournalError(e.to_string()))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        permit.send(QueuedMutation {
            mutation: SessionMutation::ToolJournal { event, ack: tx },
            _pin: Some(pin),
        });
        rx.await
            .map_err(|_| invalid("tool journal acknowledgment dropped"))?
    }
}

#[async_trait]
impl ToolExecutionJournal for SessionStateCoordinator {
    async fn recover_session(
        &self,
        session_id: SessionId,
    ) -> Result<ToolJournalRecovery, ToolJournalError> {
        if session_id != self.state.session_id {
            return Err(invalid("tool recovery belongs to a different session"));
        }
        // One projection lock gives a consistent execution/receipt boundary.
        let projection = self
            .state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if projection.latest.is_none() {
            return Err(invalid(
                "tool recovery requires a hydrated authoritative ledger",
            ));
        }
        let mut executions = projection
            .tools
            .executions
            .values()
            .cloned()
            .map(ToolExecutionRecord::recovery_view)
            .collect::<Vec<_>>();
        executions.sort_by_key(ToolExecutionRecord::execution_id);
        let mut receipts = projection
            .tools
            .receipts
            .values()
            .cloned()
            .map(NativeReceiptRecord::recovery_view)
            .collect::<Vec<_>>();
        receipts.sort_by(|a, b| a.receipt_id.cmp(&b.receipt_id));
        Ok(ToolJournalRecovery {
            executions,
            receipts,
        })
    }

    async fn record_execution(
        &self,
        record: ToolExecutionRecord,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        self.append_tool_event(SessionEvent::ToolExecution(record))
            .await
    }
    async fn execution(
        &self,
        execution_id: &str,
    ) -> Result<Option<ToolExecutionRecord>, ToolJournalError> {
        Ok(self
            .state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .executions
            .get(execution_id)
            .cloned())
    }
    async fn record_receipt(
        &self,
        record: NativeReceiptRecord,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        self.append_tool_event(SessionEvent::NativeReceipt(record))
            .await
    }
    async fn receipt(
        &self,
        receipt_id: &str,
    ) -> Result<Option<NativeReceiptRecord>, ToolJournalError> {
        Ok(self
            .state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .receipts
            .get(receipt_id)
            .cloned())
    }
}

#[async_trait]
impl ToolExecutionJournal for SessionStateManager {
    async fn recover_session(
        &self,
        session_id: SessionId,
    ) -> Result<ToolJournalRecovery, ToolJournalError> {
        self.ensure_coordinator(session_id)
            .await
            .map_err(|e| ToolJournalError(e.to_string()))?
            .recover_session(session_id)
            .await
    }

    async fn record_execution(
        &self,
        record: ToolExecutionRecord,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        self.ensure_coordinator(record.identity.session_id)
            .await
            .map_err(|e| ToolJournalError(e.to_string()))?
            .record_execution(record)
            .await
    }
    async fn execution(
        &self,
        execution_id: &str,
    ) -> Result<Option<ToolExecutionRecord>, ToolJournalError> {
        for id in self.session_ids() {
            if let Some(core) = self.coordinator(id) {
                if let Some(record) = core.execution(execution_id).await? {
                    return Ok(Some(record));
                }
            }
        }
        Ok(None)
    }
    async fn record_receipt(
        &self,
        record: NativeReceiptRecord,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        self.ensure_coordinator(record.session_id)
            .await
            .map_err(|e| ToolJournalError(e.to_string()))?
            .record_receipt(record)
            .await
    }
    async fn receipt(
        &self,
        receipt_id: &str,
    ) -> Result<Option<NativeReceiptRecord>, ToolJournalError> {
        for id in self.session_ids() {
            if let Some(core) = self.coordinator(id) {
                if let Some(record) = core.receipt(receipt_id).await? {
                    return Ok(Some(record));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::{
        NativeContinuationBinding, ToolExecutionIdentity, ToolExecutionOutcome,
    };

    struct TestLease(String);
    impl lingxi_core::host::SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }
    fn open(path: &Path, id: SessionId) -> Arc<SessionStateCoordinator> {
        SessionStateCoordinator::open(path, id, Arc::new(TestLease(id.to_string()))).unwrap()
    }
    fn started(id: SessionId) -> ToolExecutionRecord {
        ToolExecutionRecord {
            identity: ToolExecutionIdentity {
                session_id: id,
                provider_response_id: "response-1".into(),
                provider_call_id: "call-1".into(),
                member_index: 0,
            },
            tool_name: "computer".into(),
            input_digest: "sha256-final-approved-input".into(),
            recovery_binding: output(),
            acknowledged_safety_checks: vec![],
            stage: ToolExecutionStage::Started,
            outcome: None,
            output: None,
        }
    }
    fn output() -> DurableToolOutput {
        DurableToolOutput {
            digest: "sha256-post-hook-output".into(),
            payload: serde_json::json!({"transcript_path":"/session/transcript.jsonl", "message_uuid":"final-output", "text":"clicked"}),
            media_refs: vec![],
        }
    }
    fn persist(
        coordinator: &SessionStateCoordinator,
        record: ToolExecutionRecord,
    ) -> ToolJournalAck {
        coordinator
            .state
            .persist_tool_event(SessionEvent::ToolExecution(record))
            .unwrap()
    }
    fn published(coordinator: &SessionStateCoordinator, id: SessionId) -> ToolExecutionRecord {
        let mut record = started(id);
        persist(coordinator, record.clone());
        record.stage = ToolExecutionStage::Terminal;
        record.outcome = Some(ToolExecutionOutcome::Succeeded);
        persist(coordinator, record.clone());
        record.stage = ToolExecutionStage::OutputPrepared;
        record.output = Some(output());
        persist(coordinator, record.clone());
        record.stage = ToolExecutionStage::OutputPublished;
        persist(coordinator, record.clone());
        record
    }
    fn prepared(id: SessionId, execution: &ToolExecutionRecord) -> NativeReceiptRecord {
        NativeReceiptRecord {
            response: None,
            session_id: id,
            receipt_id: "response-1/call-1".into(),
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
            receipt: output(),
            provider_response_id: None,
        }
    }

    #[tokio::test]
    async fn live_queries_preserve_started_and_submitted_until_fresh_startup() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        let worker = core.start().await.unwrap();
        let mut in_flight = started(id);
        in_flight.identity.member_index = 1;
        core.record_execution(in_flight.clone()).await.unwrap();
        assert_eq!(
            core.execution(&in_flight.execution_id()).await.unwrap(),
            Some(in_flight.clone())
        );

        let completed = published(&core, id);
        let mut receipt = prepared(id, &completed);
        core.record_receipt(receipt.clone()).await.unwrap();
        receipt.stage = NativeReceiptStage::Submitted;
        receipt.submission_attempt += 1;
        core.record_receipt(receipt.clone()).await.unwrap();
        assert_eq!(
            core.receipt(&receipt.receipt_id).await.unwrap(),
            Some(receipt.clone())
        );
        // Merely reading startup recovery views does not rewrite live states.
        let view = core.recover_session(id).await.unwrap();
        assert!(view
            .executions
            .iter()
            .any(|r| r.execution_id() == in_flight.execution_id()
                && r.stage == ToolExecutionStage::OutcomeUnknown));
        assert_eq!(
            view.receipts[0].stage,
            NativeReceiptStage::SubmissionUnknown
        );
        assert_eq!(
            core.execution(&in_flight.execution_id())
                .await
                .unwrap()
                .unwrap()
                .stage,
            ToolExecutionStage::Started
        );
        assert_eq!(
            core.receipt(&receipt.receipt_id)
                .await
                .unwrap()
                .unwrap()
                .stage,
            NativeReceiptStage::Submitted
        );
        core.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(core);

        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened
                .execution(&in_flight.execution_id())
                .await
                .unwrap()
                .unwrap()
                .stage,
            ToolExecutionStage::OutcomeUnknown
        );
        assert_eq!(
            reopened
                .receipt(&receipt.receipt_id)
                .await
                .unwrap()
                .unwrap()
                .stage,
            NativeReceiptStage::SubmissionUnknown
        );
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn unsent_receipts_and_confirmed_execution_facts_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        let worker = core.start().await.unwrap();
        let mut record = started(id);
        let check = serde_json::json!({"decision":"require_confirmation","explanation":"submit"});
        record.acknowledged_safety_checks = vec![check.clone()];
        core.record_execution(record.clone()).await.unwrap();
        let mut altered = record.clone();
        altered.stage = ToolExecutionStage::Terminal;
        altered.outcome = Some(ToolExecutionOutcome::Succeeded);
        altered.acknowledged_safety_checks.clear();
        let mut projection = ToolProjection::default();
        projection
            .executions
            .insert(record.execution_id(), record.clone());
        assert!(
            projection.validate_execution(&altered, id).is_err(),
            "confirmation cannot be altered after admission"
        );
        record.stage = ToolExecutionStage::Terminal;
        record.outcome = Some(ToolExecutionOutcome::Succeeded);
        core.record_execution(record.clone()).await.unwrap();
        record.stage = ToolExecutionStage::OutputPrepared;
        record.output = Some(output());
        core.record_execution(record.clone()).await.unwrap();
        record.stage = ToolExecutionStage::OutputPublished;
        core.record_execution(record.clone()).await.unwrap();
        let mut receipt = prepared(id, &record);
        core.record_receipt(receipt.clone()).await.unwrap();
        receipt.stage = NativeReceiptStage::Submitted;
        receipt.submission_attempt = 1;
        core.record_receipt(receipt.clone()).await.unwrap();
        receipt.stage = NativeReceiptStage::NotSubmitted;
        core.record_receipt(receipt.clone()).await.unwrap();
        core.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(core);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened
                .execution(&record.execution_id())
                .await
                .unwrap()
                .unwrap()
                .acknowledged_safety_checks,
            vec![check]
        );
        assert_eq!(
            reopened.receipt(&receipt.receipt_id).await.unwrap(),
            Some(receipt.clone())
        );
        receipt.stage = NativeReceiptStage::Submitted;
        receipt.submission_attempt = 2;
        assert!(
            !reopened
                .record_receipt(receipt.clone())
                .await
                .unwrap()
                .duplicate
        );
        assert!(reopened.record_receipt(receipt).await.unwrap().duplicate);
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn prepared_complete_response_and_recovery_binding_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        let worker = core.start().await.unwrap();
        let execution = published(&core, id);
        let mut receipt = prepared(id, &execution);
        core.record_receipt(receipt.clone()).await.unwrap();
        receipt.stage = NativeReceiptStage::Submitted;
        receipt.submission_attempt = 1;
        core.record_receipt(receipt.clone()).await.unwrap();
        receipt.stage = NativeReceiptStage::ResponsePrepared;
        receipt.response = Some(output());
        receipt.provider_response_id = Some("successor".into());
        core.record_receipt(receipt.clone()).await.unwrap();
        core.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(core);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened.receipt(&receipt.receipt_id).await.unwrap(),
            Some(receipt.clone())
        );
        assert_eq!(
            reopened
                .execution(&execution.execution_id())
                .await
                .unwrap()
                .unwrap()
                .recovery_binding,
            execution.recovery_binding
        );
        let mut altered = receipt.clone();
        altered.stage = NativeReceiptStage::ResponseReceived;
        altered.response.as_mut().unwrap().digest = "changed-response".into();
        let mut projection = ToolProjection::default();
        projection
            .executions
            .insert(execution.execution_id(), execution.clone());
        projection
            .receipts
            .insert(receipt.receipt_id.clone(), receipt.clone());
        assert!(projection.validate_receipt(&altered, id).is_err());
        receipt.stage = NativeReceiptStage::ResponseReceived;
        reopened.record_receipt(receipt).await.unwrap();
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn started_ack_loss_and_reopen_never_authorize_second_input() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        core.hydrate_blocking().unwrap();
        let record = started(id);
        // Side effect followed by process loss, with no terminal WAL entry.
        let ack = persist(&core, record.clone());
        let mut input_count = u32::from(!ack.duplicate);
        drop(core);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened
                .execution(&record.execution_id())
                .await
                .unwrap()
                .unwrap()
                .stage,
            ToolExecutionStage::OutcomeUnknown
        );
        let recovery = reopened.recover_session(id).await.unwrap();
        assert_eq!(recovery.executions.len(), 1);
        assert_eq!(
            recovery.executions[0].stage,
            ToolExecutionStage::OutcomeUnknown
        );
        assert!(recovery.receipts.is_empty());
        assert!(reopened.recover_session(SessionId::new()).await.is_err());
        let retry = reopened.record_execution(record.clone()).await.unwrap();
        input_count += u32::from(!retry.duplicate);
        assert_eq!(input_count, 1);
        assert!(retry.duplicate);
        assert_eq!(retry.journal_revision, ack.journal_revision);
        let mut illegal_completion = record.clone();
        illegal_completion.stage = ToolExecutionStage::Terminal;
        illegal_completion.outcome = Some(ToolExecutionOutcome::Succeeded);
        assert!(reopened.record_execution(illegal_completion).await.is_err());
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
        let hydrated = hydrate_from_journal(&reopened.journal(), id).unwrap();
        assert_eq!(
            hydrated.tools.executions[&record.execution_id()].stage,
            ToolExecutionStage::OutcomeUnknown
        );
    }

    #[tokio::test]
    async fn cancelled_started_ack_waiter_does_not_drop_accepted_wal_write() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        let worker = core.start().await.unwrap();
        let record = started(id);
        let (permit, pin) = core.acquire_session_mutation_permit().await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        permit.send(QueuedMutation {
            mutation: SessionMutation::ToolJournal {
                event: SessionEvent::ToolExecution(record.clone()),
                ack: tx,
            },
            _pin: Some(pin),
        });
        drop(rx);
        core.flush().await.unwrap();
        assert!(core
            .journal()
            .find_event_durable(&record.event_id())
            .unwrap()
            .is_some());
        core.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(core);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened
                .execution(&record.execution_id())
                .await
                .unwrap()
                .unwrap()
                .stage,
            ToolExecutionStage::OutcomeUnknown
        );
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn completed_unpublished_output_reopens_for_publication_without_input() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        core.hydrate_blocking().unwrap();
        let mut record = started(id);
        persist(&core, record.clone());
        record.stage = ToolExecutionStage::Terminal;
        record.outcome = Some(ToolExecutionOutcome::Succeeded);
        persist(&core, record.clone());
        record.stage = ToolExecutionStage::OutputPrepared;
        record.output = Some(output()); // hook removed screenshot: no image reference survives.
        persist(&core, record.clone());
        drop(core);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        let recovered = reopened
            .execution(&record.execution_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered, record);
        assert_eq!(
            reopened.recover_session(id).await.unwrap().executions,
            vec![record.clone()]
        );
        assert!(recovered.output.as_ref().unwrap().media_refs.is_empty());
        assert!(
            reopened
                .record_execution(started(id))
                .await
                .unwrap()
                .duplicate
        );
        record.stage = ToolExecutionStage::OutputPublished;
        reopened.record_execution(record.clone()).await.unwrap();
        assert!(
            reopened
                .record_execution(record.clone())
                .await
                .unwrap()
                .duplicate
        );
        reopened.flush().await.unwrap();
        let snapshot = reopened
            .journal()
            .read_snapshot::<serde_json::Value>()
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_projection_snapshot(snapshot.state)
                .unwrap()
                .tool_executions,
            vec![record]
        );
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn prepared_receipt_is_reused_and_uncertain_submission_is_never_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        core.hydrate_blocking().unwrap();
        let execution = published(&core, id);
        let prepared = prepared(id, &execution);
        core.state
            .persist_tool_event(SessionEvent::NativeReceipt(prepared.clone()))
            .unwrap();
        drop(core);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened.receipt(&prepared.receipt_id).await.unwrap(),
            Some(prepared.clone())
        );
        let recovery = reopened.recover_session(id).await.unwrap();
        assert_eq!(recovery.executions, vec![execution]);
        assert_eq!(recovery.receipts, vec![prepared.clone()]);
        assert!(
            reopened
                .record_receipt(prepared.clone())
                .await
                .unwrap()
                .duplicate
        );
        let mut submitted = prepared.clone();
        submitted.stage = NativeReceiptStage::Submitted;
        submitted.submission_attempt += 1;
        assert!(
            !reopened
                .record_receipt(submitted.clone())
                .await
                .unwrap()
                .duplicate
        );
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(reopened);
        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        let unknown = reopened
            .receipt(&submitted.receipt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unknown.stage, NativeReceiptStage::SubmissionUnknown);
        assert_eq!(
            reopened.recover_session(id).await.unwrap().receipts,
            vec![unknown.clone()]
        );
        assert_eq!(unknown.receipt, prepared.receipt);
        assert!(reopened.record_receipt(submitted).await.unwrap().duplicate);
        let mut changed = prepared;
        changed.binding.model = "different-model".into();
        assert!(reopened.record_receipt(changed).await.is_err());
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn hook_removed_image_cannot_resume_blocker_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        let worker = core.start().await.unwrap();
        // Published post-hook output contains only text and transcript refs;
        // the deleted screenshot is absent from the authoritative ledger.
        let execution = published(&core, id);
        let mut blocked = prepared(id, &execution);
        blocked.stage = NativeReceiptStage::CannotResume;
        blocked.receipt.payload = serde_json::json!({
            "error": "native continuation requires an image removed by PostToolUse",
            "final_output_refs": [execution.execution_id()]
        });
        blocked.receipt.digest = "sha256-cannot-resume-facts".into();
        assert!(
            !core
                .record_receipt(blocked.clone())
                .await
                .unwrap()
                .duplicate
        );
        assert!(
            core.record_receipt(blocked.clone())
                .await
                .unwrap()
                .duplicate
        );
        assert!(core.tool_projection_pending());
        core.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(core);

        let reopened = open(dir.path(), id);
        let worker = reopened.start().await.unwrap();
        assert_eq!(
            reopened.receipt(&blocked.receipt_id).await.unwrap(),
            Some(blocked.clone())
        );
        let recovery = reopened.recover_session(id).await.unwrap();
        assert_eq!(recovery.receipts, vec![blocked.clone()]);
        assert!(recovery.receipts[0].receipt.media_refs.is_empty());
        assert!(recovery.executions[0]
            .output
            .as_ref()
            .unwrap()
            .media_refs
            .is_empty());
        assert!(reopened.tool_projection_pending());
        // CannotResume is absorbing even if a caller keeps the original binding.
        let mut illegal = blocked;
        illegal.stage = NativeReceiptStage::Submitted;
        assert!(reopened.record_receipt(illegal).await.is_err());
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn corrupted_started_wal_reopen_preserves_evidence_and_blocks_input() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let core = open(dir.path(), id);
        core.hydrate_blocking().unwrap();
        let record = started(id);
        assert!(!persist(&core, record.clone()).duplicate);
        let wal = core
            .journal()
            .root()
            .join(session::jsonl::JOURNAL_FILE_NAME);
        let mut damaged = std::fs::read(&wal).unwrap();
        // Interior malformed line forces physical corruption rather than the
        // safely repairable final tail that never received a durable ACK.
        damaged.splice(0..0, b"{unreadable record\n".iter().copied());
        std::fs::write(&wal, &damaged).unwrap();
        drop(core);
        let reopened = open(dir.path(), id);
        assert!(reopened.start().await.is_err());
        assert!(reopened.record_execution(record).await.is_err());
        assert_eq!(std::fs::read(&wal).unwrap(), damaged);
        assert!(!reopened.journal().root().join("quarantine").exists());
    }

    #[test]
    fn strict_events_reject_inline_images_unknown_fields_and_orphan_results() {
        let id = SessionId::new();
        let core_dir = tempfile::tempdir().unwrap();
        let core = open(core_dir.path(), id);
        core.hydrate_blocking().unwrap();
        let mut terminal = started(id);
        terminal.stage = ToolExecutionStage::Terminal;
        terminal.outcome = Some(ToolExecutionOutcome::Succeeded);
        assert!(core
            .state
            .persist_tool_event(SessionEvent::ToolExecution(terminal))
            .is_err());
        let mut value = serde_json::to_value(started(id)).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(decode_session_event(serde_json::json!({"ToolExecution":value})).is_err());
        let mut image = output();
        image.payload = serde_json::json!({"source":{"type":"base64","data":"pixels"}});
        assert!(validate_output(&image).is_err());
        image.payload = serde_json::json!({"url":"data:image/png;base64,cGl4ZWxz"});
        assert!(validate_output(&image).is_err());
    }
}
