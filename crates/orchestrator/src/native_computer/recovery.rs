//! Reconstruct native work from the authoritative session ledger and saved rows.
//! Recovery publishes existing results; it never dispatches desktop input.
use super::*;
use lingxi_core::types::SessionId;
use std::collections::{BTreeSet, HashSet};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedBinding {
    #[serde(rename = "type")]
    _kind: String,
    call: NativeComputerCall,
    tool_use_ids: Vec<String>,
    original_blocks: Vec<wire::ContentBlock>,
    frame: ComputerFrame,
    binding: NativeContinuationBinding,
    provider_response_id: String,
}

fn blocks(message: &ConversationMessage) -> &[ContentBlock] {
    match message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content,
        ConversationMessage::System { .. } => &[],
    }
}

/// Keep an unsent native round's complete accepted origin and subsequent rows
/// in the ordinary compaction tail. Its boundary cannot be reconstructed from
/// receipt artifacts, and submitted/completed rounds must not be resurrected.
pub(super) async fn preserve_pending_compaction_tail(
    orch: &ConversationOrchestrator,
    preserved: &[ConversationMessage],
) -> Result<Vec<ConversationMessage>, OrchestratorError> {
    let Some(journal) = &orch.tool_execution_journal else {
        return Ok(preserved.to_vec());
    };
    let session_id = orch.session.lock().await.session_id;
    let recovered = journal
        .recover_session(session_id)
        .await
        .map_err(internal)?;
    let pending: Vec<_> = recovered
        .receipts
        .iter()
        .filter(|record| {
            matches!(
                record.stage,
                NativeReceiptStage::Prepared | NativeReceiptStage::NotSubmitted
            )
        })
        .collect();
    if pending.is_empty() {
        return Ok(preserved.to_vec());
    }
    let session = orch.session.lock().await;
    if session.session_id != session_id {
        return Err(internal("compaction changed the pending receipt owner"));
    }
    let history = &session.history;
    let mut first_origin = history.len();
    for receipt in pending {
        if receipt.execution_ids.is_empty() {
            return Err(internal("pending receipt has no execution identities"));
        }
        for id in &receipt.execution_ids {
            let record = recovered
                .executions
                .iter()
                .find(|record| record.execution_id() == *id)
                .ok_or_else(|| internal("pending receipt has no durable execution facts"))?;
            let origin = history
                .iter()
                .position(|message| {
                    blocks(message).iter().any(|block| matches!(block,
                ContentBlock::ProviderContent {protocol, value}
                    if protocol == &receipt.binding.protocol
                    && value["type"] == "lingxi_computer_binding"
                    && value["provider_response_id"] == record.identity.provider_response_id
                    && value["call"]["context"]["call_id"] == record.identity.provider_call_id
            ))
                })
                .ok_or_else(|| {
                    internal("cannot compact a pending receipt with a missing native origin")
                })?;
            first_origin = first_origin.min(origin);
        }
    }
    let mut selected: HashMap<_, _> = preserved
        .iter()
        .map(|message| (message.id(), message.clone()))
        .collect();
    let mut result = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if index >= first_origin {
            // Pending protocol evidence remains the exact accepted row, even
            // if a summarizer Mod rewrote its supplied preservation list.
            selected.remove(&message.id());
            result.push(message.clone());
        } else if let Some(message) = selected.remove(&message.id()) {
            result.push(message);
        }
    }
    // Preserve additional caller rows in their original order, without duplicates.
    for message in preserved {
        if let Some(message) = selected.remove(&message.id()) {
            result.push(message);
        }
    }
    drop(session);
    let writer = orch
        .transcript
        .jsonl_writer
        .as_ref()
        .ok_or_else(|| internal("pending compaction tail has no transcript writer"))?;
    let path = writer
        .session_target_path(session_id)
        .ok_or_else(|| internal("pending compaction tail has no session transcript"))?;
    // This existing API intersects the identity log with complete physical
    // JSONL rows, so registration before a failed append is not publication.
    let persisted = writer
        .read_session_message_identity_snapshot(&path)
        .await
        .map_err(internal)?;
    for message in &result {
        if !persisted
            .by_uuid
            .contains_key(&message.id().as_uuid().to_string())
        {
            // Recovery can derive notices only in memory. A preserved tail's
            // UUIDs must all exist on disk before the compact boundary names it.
            let row = prepare_stored_row(orch, message).await?;
            persist_stored_row(
                orch,
                &row,
                &format!("native-compact-tail:{session_id}:{}", message.id()),
            )
            .await?;
        }
    }
    Ok(result)
}

fn member_id(identity: &ToolExecutionIdentity) -> ToolUseId {
    ToolUseId::from(format!(
        "native_computer_{}",
        identity
            .execution_id()
            .trim_start_matches("tool-execution:")
    ))
}

async fn restore_execution_bindings(
    records: &HashMap<String, ToolExecutionRecord>,
    tool: &Arc<dyn Tool>,
    work: &mut HashMap<ToolUseId, Work>,
) -> Result<Option<NativeContinuationBinding>, OrchestratorError> {
    let mut fallback = None;
    for record in records.values() {
        let saved: StoredExecutionBinding =
            serde_json::from_value(load_payload(&record.recovery_binding).await?)
                .map_err(internal)?;
        let index = usize::try_from(record.identity.member_index).map_err(internal)?;
        if saved.provider_response_id != record.identity.provider_response_id
            || saved.call.context.call_id != record.identity.provider_call_id
            || index >= saved.call.operations.len()
            || saved.binding.protocol != protocol_name(saved.call.context.provider)
            || record.tool_name != tool.name()
        {
            return Err(internal(
                "durable execution recovery binding does not match its identity",
            ));
        }
        let id = member_id(&record.identity);
        let restored = Work {
            call: Arc::new(saved.call),
            index,
            identity: record.identity.clone(),
            frame: saved.frame,
            tool: tool.clone(),
            binding: saved.binding.clone(),
        };
        if let Some(previous) = work.get(&id) {
            if previous.binding != restored.binding
                || previous.frame != restored.frame
                || previous.call != restored.call
            {
                return Err(internal(
                    "execution recovery binding contradicts retained history",
                ));
            }
        } else {
            work.insert(id, restored);
        }
        fallback = Some(saved.binding);
    }
    Ok(fallback)
}

/// Complete exact, ledger-backed responses without repeating input or model submission.
async fn finish_prepared_responses(
    orch: &ConversationOrchestrator,
    recovered: &mut lingxi_core::host::ToolJournalRecovery,
    session_id: SessionId,
) -> Result<(), OrchestratorError> {
    if !recovered
        .receipts
        .iter()
        .any(|r| r.stage == NativeReceiptStage::ResponsePrepared)
    {
        return Ok(());
    }
    let journal = orch
        .tool_execution_journal
        .as_ref()
        .ok_or_else(|| internal("response recovery requires journal"))?;
    let proofs = recovered
        .receipts
        .iter()
        .filter(|r| r.stage == NativeReceiptStage::ResponsePrepared)
        .cloned()
        .collect::<Vec<_>>();
    let mut processed = HashSet::new();
    for proof in proofs {
        let output = proof
            .response
            .as_ref()
            .ok_or_else(|| internal("prepared response has no complete saved row"))?;
        if !processed.insert(output.digest.clone()) {
            continue;
        }
        let stored = load_output(output).await?;
        let response_id = proof
            .provider_response_id
            .as_deref()
            .ok_or_else(|| internal("prepared response has no provider identity"))?;
        if stored.row.session_id != session_id.as_uuid().to_string()
            || stored.row.message_type != "assistant"
            || stored.row.message["id"] != response_id
            || !matches!(stored.message, ConversationMessage::Assistant { .. })
        {
            return Err(internal(
                "prepared successor belongs to another session or role",
            ));
        }
        let ack = blocks(&stored.message)
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ProviderContent { protocol, value }
                    if value["type"] == "lingxi_computer_receipt_ack"
                        && value["provider_response_id"] == response_id
                        && *protocol == proof.binding.protocol =>
                {
                    Some(value)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if ack.len() != 1 {
            return Err(internal(
                "prepared successor has no exact receipt acknowledgement",
            ));
        }
        let associations: Vec<ReceiptAcknowledgement> =
            serde_json::from_value(ack[0]["receipts"].clone()).map_err(internal)?;
        if associations.is_empty()
            || !associations.contains(&ReceiptAcknowledgement::from_record(&proof))
        {
            return Err(internal(
                "prepared successor does not acknowledge its receipt attempt",
            ));
        }
        let mut ids = HashSet::new();
        let mut acknowledged_calls = std::collections::BTreeSet::new();
        for association in &associations {
            let record = recovered
                .receipts
                .iter()
                .find(|r| r.receipt_id == association.receipt_id)
                .ok_or_else(|| internal("successor references missing receipt"))?;
            if !ids.insert(association.receipt_id.clone())
                || ReceiptAcknowledgement::from_record(record) != *association
                || record.session_id != session_id
                || record.binding != proof.binding
                || !matches!(
                    record.stage,
                    NativeReceiptStage::Submitted
                        | NativeReceiptStage::SubmissionUnknown
                        | NativeReceiptStage::ResponsePrepared
                        | NativeReceiptStage::ResponseReceived
                )
                || (record.response.is_some() && record.response.as_ref() != Some(output))
                || (record.provider_response_id.is_some()
                    && record.provider_response_id.as_deref() != Some(response_id))
            {
                return Err(internal(
                    "successor changed receipt scope, attempt or complete response",
                ));
            }
            for id in &record.execution_ids {
                let execution = recovered
                    .executions
                    .iter()
                    .find(|e| e.execution_id() == *id)
                    .ok_or_else(|| internal("acknowledged execution is missing"))?;
                acknowledged_calls.insert(execution.identity.provider_call_id.clone());
            }
        }
        let call_ids: std::collections::BTreeSet<String> =
            serde_json::from_value(ack[0]["call_ids"].clone()).map_err(internal)?;
        if call_ids != acknowledged_calls {
            return Err(internal("successor acknowledged different computer calls"));
        }
        // All associated receipts reach ResponsePrepared before any row repair.
        for record in recovered
            .receipts
            .iter_mut()
            .filter(|r| ids.contains(&r.receipt_id))
        {
            if matches!(
                record.stage,
                NativeReceiptStage::Submitted | NativeReceiptStage::SubmissionUnknown
            ) {
                record.stage = NativeReceiptStage::ResponsePrepared;
                record.provider_response_id = Some(response_id.into());
                record.response = Some(output.clone());
                journal
                    .record_receipt(record.clone())
                    .await
                    .map_err(internal)?;
            }
        }
        persist_stored_row(orch, &stored.row, &format!("native-response:{response_id}")).await?;
        restore_message(orch, stored.message).await;
        for record in recovered
            .receipts
            .iter_mut()
            .filter(|r| ids.contains(&r.receipt_id))
        {
            if record.stage == NativeReceiptStage::ResponsePrepared {
                record.stage = NativeReceiptStage::ResponseReceived;
                journal
                    .record_receipt(record.clone())
                    .await
                    .map_err(internal)?;
            }
        }
    }
    Ok(())
}

fn add_binding(
    saved: SavedBinding,
    session_id: SessionId,
    tool: &Arc<dyn Tool>,
    work: &mut HashMap<ToolUseId, Work>,
) -> Result<(), OrchestratorError> {
    if saved.tool_use_ids.len() != saved.call.operations.len()
        || saved.original_blocks.is_empty()
        || saved
            .original_blocks
            .iter()
            .any(|block| computer_call_id(block) != Some(saved.call.context.call_id.as_str()))
        || saved.provider_response_id.trim().is_empty()
        || saved.binding.protocol != protocol_name(saved.call.context.provider)
    {
        return Err(internal(
            "saved computer binding is incomplete or inconsistent",
        ));
    }
    let call = Arc::new(saved.call);
    for (index, id) in saved.tool_use_ids.iter().enumerate() {
        let identity = ToolExecutionIdentity {
            session_id,
            provider_response_id: saved.provider_response_id.clone(),
            provider_call_id: call.context.call_id.clone(),
            member_index: u32::try_from(index).map_err(internal)?,
        };
        let expected = member_id(&identity);
        if expected.as_str() != id.as_str() {
            return Err(internal("saved native member identity changed"));
        }
        let member = Work {
            call: call.clone(),
            index,
            identity,
            frame: saved.frame.clone(),
            tool: tool.clone(),
            binding: saved.binding.clone(),
        };
        if let Some(previous) = work.get(&expected) {
            if previous.identity != member.identity || previous.binding != member.binding {
                return Err(internal("conflicting saved computer bindings"));
            }
        } else {
            work.insert(expected, member);
        }
    }
    Ok(())
}

fn restore_receipt_work(
    stored: &StoredReceipt,
    receipt: &NativeReceiptRecord,
    records: &HashMap<String, ToolExecutionRecord>,
    tool: &Arc<dyn Tool>,
    work: &mut HashMap<ToolUseId, Work>,
) -> Result<(), OrchestratorError> {
    let call = Arc::new(stored.call.clone());
    for execution_id in &receipt.execution_ids {
        let record = records
            .get(execution_id)
            .ok_or_else(|| internal("saved receipt references missing execution facts"))?;
        let index = usize::try_from(record.identity.member_index).map_err(internal)?;
        if record.identity.session_id != receipt.session_id
            || record.identity.provider_call_id != stored.call.context.call_id
            || index >= stored.call.operations.len()
            || receipt.binding.protocol != protocol_name(stored.call.context.provider)
        {
            return Err(internal("saved receipt association is invalid"));
        }
        let id = member_id(&record.identity);
        let restored = Work {
            call: call.clone(),
            index,
            identity: record.identity.clone(),
            frame: stored.frame.clone(),
            tool: tool.clone(),
            binding: receipt.binding.clone(),
        };
        if let Some(previous) = work.get(&id) {
            if previous.identity != restored.identity || previous.binding != restored.binding {
                return Err(internal("receipt contradicts its durable native binding"));
            }
        } else {
            work.insert(id, restored);
        }
    }
    Ok(())
}

async fn restore_message(orch: &ConversationOrchestrator, message: ConversationMessage) {
    let mut session = orch.session.lock().await;
    if !session
        .history
        .iter()
        .any(|existing| existing.id() == message.id())
    {
        session.history.push(message);
    }
}

fn recovery_message_id(
    session_id: SessionId,
    kind: &str,
    calls: &BTreeSet<(String, String)>,
    origins: &BTreeSet<String>,
) -> Result<MessageId, OrchestratorError> {
    let hash = digest(&json!([session_id, kind, calls, origins]))?;
    let uuid = format!(
        "{}-{}-4{}-a{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[13..16],
        &hash[17..20],
        &hash[20..32]
    );
    MessageId::parse_prefixed(uuid).ok_or_else(|| internal("invalid recovery message identity"))
}

async fn restore_marker(
    orch: &ConversationOrchestrator,
    session_id: SessionId,
    kind: &str,
    protocol: &str,
    calls: &BTreeSet<(String, String)>,
    origins: &BTreeSet<String>,
    reason: Option<&str>,
) -> Result<(), OrchestratorError> {
    if calls.is_empty() {
        return Ok(());
    }
    let call_ids: BTreeSet<_> = calls.iter().map(|(_, call)| call).collect();
    let mut value = json!({"type":kind,"call_ids":call_ids,"call_identities":calls});
    if let Some(reason) = reason {
        value["reason"] = json!(reason);
    }
    let mut content = vec![ContentBlock::ProviderContent {
        protocol: protocol.into(),
        value,
    }];
    if let Some(reason) = reason {
        content.push(ContentBlock::Text {
            text: reason.into(),
            citations: None,
        });
    }
    // This view is derived from execution facts and scoped by response identity.
    restore_message(
        orch,
        ConversationMessage::User { api_message_override: None,
            id: recovery_message_id(session_id, kind, calls, origins)?,
            content,
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        },
    )
    .await;
    Ok(())
}

/// Abandon an unanswerable native call while retaining ordinary execution facts.
/// This is a deterministic ledger projection, never another desktop action.
pub(super) async fn abandon_calls(
    orch: &ConversationOrchestrator,
    session_id: SessionId,
    provider: NativeComputerProvider,
    calls: BTreeSet<(String, String)>,
    reason: &str,
) -> Result<(), OrchestratorError> {
    if calls.is_empty() {
        return Ok(());
    }
    let history = orch.session.lock().await.history.clone();
    let members = orch
        .computer_runtime
        .work
        .lock()
        .await
        .values()
        .filter(|member| calls.contains(&member.identity.call_identity()))
        .cloned()
        .collect::<Vec<_>>();
    let journal = orch
        .tool_execution_journal
        .as_ref()
        .ok_or_else(|| internal("abandoned call has no durable journal"))?;
    let mut origins: BTreeSet<_> = calls.iter().map(|(response, _)| response.clone()).collect();
    for member in members {
        let fact = journal
            .execution(&member.identity.execution_id())
            .await
            .map_err(internal)?;
        if fact.as_ref().is_none_or(needs_fresh_observation) {
            origins.insert(member.identity.provider_response_id);
        }
    }
    let marker_id = recovery_message_id(session_id, "lingxi_computer_abandoned", &calls, &origins)?;
    let new_calls = !history.iter().any(|message| message.id() == marker_id);
    let invalidate = {
        let mut state = orch.computer_runtime.state.lock().await;
        if state.session_id != Some(session_id) {
            return Err(internal(
                "abandoned call belongs to a different owner session",
            ));
        }
        if let Some(reason) = receipt_blocker(&state.pending_receipts) {
            return Err(internal(reason));
        }
        let unsafe_origin = state
            .continuation
            .as_ref()
            .is_some_and(|reference| origins.contains(reference.response_id.as_str()));
        // An old Unknown audit record must not invalidate a newer function
        // observation on every later prepare. Only the original continuation
        // and the first abandoned projection require this reset.
        if new_calls || unsafe_origin {
            state.continuation = None;
            state.next_native = false;
            state.active = None;
            state.fallback_reason = Some(reason.into());
        }
        new_calls || unsafe_origin
    };
    restore_marker(
        orch,
        session_id,
        "lingxi_computer_abandoned",
        &protocol_name(provider),
        &calls,
        &origins,
        Some(reason),
    )
    .await?;
    if invalidate {
        if let Some(tool) = orch
            .tools
            .find_registered("computer")
            .filter(|tool| !tool.is_mcp() && tool.native_computer_capabilities().is_some())
        {
            let mut ctx = crate::turn_loop::streaming_tool_context_base(orch, Vec::new()).await;
            ctx.origin_session_id = Some(session_id);
            tool.invalidate_computer_observation(&ctx)
                .await
                .map_err(internal)?;
        }
    }
    Ok(())
}

fn abandonment_provider(
    work: &HashMap<ToolUseId, Work>,
    calls: &BTreeSet<(String, String)>,
    binding: Option<&NativeContinuationBinding>,
) -> Result<NativeComputerProvider, OrchestratorError> {
    if let Some(member) = work
        .values()
        .find(|member| calls.contains(&member.identity.call_identity()))
    {
        return Ok(member.call.context.provider);
    }
    match binding.map(|binding| binding.protocol.as_str()) {
        Some("open_ai_responses") => Ok(NativeComputerProvider::OpenAi),
        Some("anthropic_messages") => Ok(NativeComputerProvider::Anthropic),
        Some("gemini_interactions") => Ok(NativeComputerProvider::Gemini),
        _ => Err(internal(
            "unknown execution has no trusted provider for fresh observation",
        )),
    }
}

fn needs_fresh_observation(record: &ToolExecutionRecord) -> bool {
    record.outcome == Some(ToolExecutionOutcome::Unknown)
        || !matches!(
            record.stage,
            ToolExecutionStage::OutputPrepared | ToolExecutionStage::OutputPublished
        )
}

fn compacted_abandoned_output(
    record: &ToolExecutionRecord,
    records: &HashMap<String, ToolExecutionRecord>,
    history: &[ConversationMessage],
) -> bool {
    !history.iter().flat_map(blocks).any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == &member_id(&record.identity)))
        && records.values().any(|member| member.identity.call_identity() == record.identity.call_identity() && needs_fresh_observation(member))
}

fn validate_receipt_row(
    stored: &StoredReceipt,
    record: &NativeReceiptRecord,
    session_id: SessionId,
) -> Result<(), OrchestratorError> {
    if stored.row.session_id != session_id.as_uuid().to_string()
        || !blocks(&stored.message).iter().any(|block| matches!(block,
            ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_computer_receipt"
                && value["receipt_id"] == record.receipt_id
                && value["call_id"] == stored.call.context.call_id
                && value["block"] == serde_json::to_value(&stored.block).expect("stored block serializes")))
    {
        return Err(internal("saved receipt row does not match its durable association"));
    }
    Ok(())
}

/// Completed ledger facts are an audit source, not an alternate conversation
/// history. Compaction may intentionally remove their messages and artifacts.
fn restrict_completed_recovery(
    recovered: &mut lingxi_core::host::ToolJournalRecovery,
    history: &[ConversationMessage],
    live: bool,
) -> Result<(), OrchestratorError> {
    let completed: HashSet<_> = recovered
        .receipts
        .iter()
        .filter(|r| r.stage == NativeReceiptStage::ResponseReceived)
        .flat_map(|r| r.execution_ids.iter().cloned())
        .collect();
    let identities: HashMap<_, _> = recovered
        .executions
        .iter()
        .map(|r| (r.execution_id(), r.identity.call_identity()))
        .collect();
    let current_calls: HashSet<_> = history
        .iter()
        .flat_map(blocks)
        .filter_map(|block| match block {
            ContentBlock::ProviderContent { value, .. }
                if value["type"] == "lingxi_computer_binding" =>
            {
                let call = value
                    .pointer("/call/context/call_id")
                    .and_then(Value::as_str)?;
                let response = value["provider_response_id"].as_str()?;
                Some((response.to_owned(), call.to_owned()))
            }
            _ => None,
        })
        .collect();
    let mut retained_receipts = Vec::new();
    for record in &recovered.receipts {
        if record.stage != NativeReceiptStage::ResponseReceived {
            retained_receipts.push(record.clone());
            continue;
        }
        // Live refresh repairs unfinished publication only. An absent message
        // ID cannot distinguish a lost append from intentional compaction.
        if live {
            continue;
        }
        let calls: HashSet<_> = record
            .execution_ids
            .iter()
            .filter_map(|id| identities.get(id).cloned())
            .collect();
        if calls.is_disjoint(&current_calls) {
            continue;
        }
        let response_id = record
            .provider_response_id
            .as_deref()
            .ok_or_else(|| internal("received receipt has no durable successor identity"))?;
        let successor_in_history = history.iter().any(|message| {
            let ConversationMessage::Assistant { content, .. } = message else {
                return false;
            };
            content.iter().any(|block| {
                let ContentBlock::ProviderContent { value, .. } = block else {
                    return false;
                };
                (value["type"] == "lingxi_computer_continuation"
                    && value
                        .pointer("/continuation/response_id")
                        .and_then(Value::as_str)
                        == Some(response_id))
                    || (value["type"] == "lingxi_computer_receipt_ack"
                        && value["provider_response_id"].as_str() == Some(response_id)
                        && value["call_ids"].as_array().is_some_and(|ids| {
                            ids.iter().any(|id| {
                                id.as_str()
                                    .is_some_and(|id| calls.iter().any(|(_, call)| call == id))
                            })
                        }))
            })
        });
        if !successor_in_history {
            return Err(internal("received computer receipt has no durable successor assistant boundary; incomplete legacy recovery cannot resume"));
        }
        retained_receipts.push(record.clone());
    }
    let retained_completed: HashSet<_> = retained_receipts
        .iter()
        .filter(|r| r.stage == NativeReceiptStage::ResponseReceived)
        .flat_map(|r| r.execution_ids.iter().cloned())
        .collect();
    recovered.executions.retain(|r| {
        !completed.contains(&r.execution_id()) || retained_completed.contains(&r.execution_id())
    });
    recovered.receipts = retained_receipts;
    Ok(())
}

/// Validate real successor references only. A ledger response ID cannot stand
/// in for the assistant message/body that received it or invent a wire boundary.
fn validate_received_boundaries(
    history: &[ConversationMessage],
    artifacts: &[(NativeReceiptRecord, StoredReceipt)],
) -> Result<(), OrchestratorError> {
    let mut successors = HashMap::new();
    for (record, stored) in artifacts
        .iter()
        .filter(|(r, _)| r.stage == NativeReceiptStage::ResponseReceived)
    {
        let Some(origin) = stored.call.context.continuation.as_ref() else {
            continue;
        };
        if origin.account_scope != record.binding.account
            || origin.profile_name != record.binding.profile
            || origin.request_model != record.binding.model
            || origin.endpoint_fingerprint != record.binding.endpoint
            || serde_json::to_value(origin.protocol)
                .map_err(internal)?
                .as_str()
                != Some(record.binding.protocol.as_str())
        {
            return Err(internal(
                "receipt successor changed its original continuation scope",
            ));
        }
        let response_id = record
            .provider_response_id
            .as_deref()
            .ok_or_else(|| internal("received receipt has no successor response"))?;
        let key = digest(&serde_json::to_value(origin).map_err(internal)?)?;
        if let Some(previous) = successors.insert(key, response_id) {
            if previous != response_id {
                return Err(internal(
                    "conflicting received successors for one computer continuation",
                ));
            }
        }
        for value in history
            .iter()
            .flat_map(blocks)
            .filter_map(|block| match block {
                ContentBlock::ProviderContent { value, .. }
                    if value["type"] == "lingxi_computer_continuation" =>
                {
                    Some(value)
                }
                _ => None,
            })
        {
            let reference: wire::ContinuationRef =
                serde_json::from_value(value["continuation"].clone()).map_err(internal)?;
            if reference.response_id.as_str() == response_id {
                let mut expected = origin.clone();
                expected.response_id = wire::ResponseId::new(response_id);
                if reference != expected {
                    return Err(internal(
                        "durable successor assistant changed its continuation scope",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn receipt_blocker(records: &[NativeReceiptRecord]) -> Option<&'static str> {
    records
        .iter()
        .find(|record| {
            matches!(
                record.stage,
                NativeReceiptStage::Submitted
                    | NativeReceiptStage::SubmissionUnknown
                    | NativeReceiptStage::CannotResume
            )
        })
        .map(|record| {
            if record.stage == NativeReceiptStage::CannotResume {
                "final hook-visible computer output cannot resume the native provider call"
            } else {
                "native provider submission outcome is unknown; automatic resubmission is refused"
            }
        })
}

/// Refresh ledger-backed publication failures before every request. This keeps
/// the live round's readiness/active binding intact while discovering receipts
/// whose durable Prepared ACK preceded a failed transcript/pending-list append.
pub(super) async fn refresh_pending(
    orch: &ConversationOrchestrator,
    session_id: SessionId,
) -> Result<(), OrchestratorError> {
    let Some(journal) = &orch.tool_execution_journal else {
        return Ok(());
    };
    let mut recovered = journal
        .recover_session(session_id)
        .await
        .map_err(internal)?;
    finish_prepared_responses(orch, &mut recovered, session_id).await?;
    let history = orch.session.lock().await.history.clone();
    // Live completed facts are intentionally removed below. Their cached work
    // must not then look like accepted calls that never reached Started.
    let receipted: HashSet<_> = recovered
        .receipts
        .iter()
        .flat_map(|record| record.execution_ids.iter().cloned())
        .collect();
    restrict_completed_recovery(&mut recovered, &history, true)?;
    let records: HashMap<_, _> = recovered
        .executions
        .into_iter()
        .map(|r| (r.execution_id(), r))
        .collect();
    let tool = orch
        .tools
        .find_registered("computer")
        .filter(|tool| !tool.is_mcp() && tool.native_computer_capabilities().is_some());
    let mut work = orch.computer_runtime.work.lock().await.clone();
    if let Some(tool) = &tool {
        restore_execution_bindings(&records, tool, &mut work).await?;
    }
    let mut artifacts = Vec::new();
    for record in &recovered.receipts {
        if record.stage == NativeReceiptStage::CannotResume {
            continue;
        }
        let stored = load_receipt(&record.receipt).await?;
        validate_receipt_row(&stored, record, session_id)?;
        let tool = tool
            .as_ref()
            .ok_or_else(|| internal("receipt recovery requires the registered builtin computer"))?;
        restore_receipt_work(&stored, record, &records, tool, &mut work)?;
        artifacts.push((record.clone(), stored));
    }
    *orch.computer_runtime.work.lock().await = work.clone();
    for record in records.values().filter(|record| {
        matches!(
            record.stage,
            ToolExecutionStage::OutputPrepared | ToolExecutionStage::OutputPublished
        )
    }) {
        if compacted_abandoned_output(record, &records, &history) {
            // Unknown audit facts survive compaction; their removed model rows do not.
            continue;
        }
        let stored = load_output(
            record
                .output
                .as_ref()
                .ok_or_else(|| internal("saved output artifact missing"))?,
        )
        .await?;
        let expected = member_id(&record.identity);
        if stored.row.session_id != session_id.as_uuid().to_string()
            || blocks(&stored.message)
                .iter()
                .filter(|block| {
                    matches!(block,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &expected)
                })
                .count()
                != 1
        {
            return Err(internal(
                "saved final output row has an invalid execution association",
            ));
        }
        if record.stage == ToolExecutionStage::OutputPrepared
            && !publish_native_result(orch, &stored.message).await?
        {
            return Err(internal(
                "unpublished native output has no restored binding",
            ));
        }
        restore_message(orch, stored.message).await;
    }
    for (record, stored) in artifacts {
        match record.stage {
            NativeReceiptStage::Prepared | NativeReceiptStage::NotSubmitted => {
                persist_stored_row(orch, &stored.row, &format!("{}:history", record.receipt_id))
                    .await?;
                restore_message(orch, stored.message).await;
            }
            _ => {}
        }
    }
    {
        let mut state = orch.computer_runtime.state.lock().await;
        if state.session_id != Some(session_id) {
            return Err(internal(
                "receipt refresh belongs to a different owner session",
            ));
        }
        state.pending_receipts = recovered
            .receipts
            .iter()
            .filter(|record| record.stage != NativeReceiptStage::ResponseReceived)
            .cloned()
            .collect();
        if state.continuation_binding.is_none() {
            state.continuation_binding = recovered
                .receipts
                .last()
                .map(|record| record.binding.clone());
        }
    }
    if let Some(reason) = receipt_blocker(&recovered.receipts) {
        orch.computer_runtime.state.lock().await.fallback_reason = Some(reason.into());
        return Err(internal(reason));
    }
    let mut abandoned: BTreeSet<_> = records
        .values()
        .filter(|record| needs_fresh_observation(record))
        .map(|record| record.identity.call_identity())
        .collect();
    abandoned.extend(
        work.values()
            .filter(|member| {
                let id = member.identity.execution_id();
                !receipted.contains(&id) && records.get(&id).is_none_or(needs_fresh_observation)
            })
            .map(|member| member.identity.call_identity()),
    );
    if !abandoned.is_empty() {
        let binding = orch
            .computer_runtime
            .state
            .lock()
            .await
            .continuation_binding
            .clone();
        let provider = abandonment_provider(&work, &abandoned, binding.as_ref())?;
        abandon_calls(orch, session_id, provider, abandoned.clone(),
            "Recovered computer execution was not durably started, has no final model-visible output, or has an unknown input outcome. Do not repeat the inputs; obtain a fresh observation through the same provider's computer function.").await?;
    }
    let mut ready = Vec::new();
    for (id, member) in &work {
        let members = work
            .values()
            .filter(|candidate| {
                candidate.identity.provider_response_id == member.identity.provider_response_id
                    && candidate.call.context.call_id == member.call.context.call_id
            })
            .collect::<Vec<_>>();
        if !receipted.contains(&member.identity.execution_id())
            && !abandoned.contains(&member.identity.call_identity())
            && members.len() == member.call.operations.len()
            && members.iter().all(|candidate| {
                records
                    .get(&candidate.identity.execution_id())
                    .is_some_and(|record| !needs_fresh_observation(record))
            })
        {
            ready.push(id.clone());
        }
    }
    ready.sort_by_key(ToString::to_string);
    if !ready.is_empty() {
        prepare_receipts(orch, &ready).await?;
    }
    Ok(())
}

pub(super) async fn rehydrate(
    orch: &ConversationOrchestrator,
    session_id: SessionId,
) -> Result<(), OrchestratorError> {
    let mut history = orch.session.lock().await.history.clone();
    let markers = history
        .iter()
        .flat_map(blocks)
        .filter_map(|block| match block {
            ContentBlock::ProviderContent { value, .. } => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let has_saved_binding = markers
        .iter()
        .any(|value| value["type"] == "lingxi_computer_binding");
    let Some(journal) = &orch.tool_execution_journal else {
        if has_saved_binding {
            return Err(internal("native recovery has no durable journal"));
        }
        *orch.computer_runtime.state.lock().await = RoundState {
            session_id: Some(session_id),
            ..Default::default()
        };
        orch.computer_runtime.work.lock().await.clear();
        return Ok(());
    };
    let mut recovered = journal
        .recover_session(session_id)
        .await
        .map_err(internal)?;
    finish_prepared_responses(orch, &mut recovered, session_id).await?;
    history = orch.session.lock().await.history.clone();
    let markers = history
        .iter()
        .flat_map(blocks)
        .filter_map(|block| match block {
            ContentBlock::ProviderContent { value, .. } => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    restrict_completed_recovery(&mut recovered, &history, false)?;
    let records: HashMap<_, _> = recovered
        .executions
        .into_iter()
        .map(|record| (record.execution_id(), record))
        .collect();
    let has_native = has_saved_binding || !records.is_empty() || !recovered.receipts.is_empty();
    let tool = orch
        .tools
        .find_registered("computer")
        .filter(|tool| !tool.is_mcp() && tool.native_computer_capabilities().is_some());
    if has_native && tool.is_none() {
        return Err(internal(
            "saved native work requires the registered builtin computer",
        ));
    }
    let mut restored = HashMap::new();
    let mut continuation = None;
    let mut continuation_binding = None;
    for value in &markers {
        match value["type"].as_str() {
            Some("lingxi_computer_binding") => {
                let saved: SavedBinding =
                    serde_json::from_value((*value).clone()).map_err(internal)?;
                continuation_binding = Some(saved.binding.clone());
                add_binding(
                    saved,
                    session_id,
                    tool.as_ref().expect("native tool checked"),
                    &mut restored,
                )?;
            }
            Some("lingxi_computer_continuation") => {
                let saved: wire::ContinuationRef =
                    serde_json::from_value(value["continuation"].clone()).map_err(internal)?;
                continuation_binding = Some(NativeContinuationBinding {
                    account: saved.account_scope.clone(),
                    profile: saved.profile_name.clone(),
                    model: saved.request_model.clone(),
                    endpoint: saved.endpoint_fingerprint.clone(),
                    protocol: serde_json::to_value(saved.protocol)
                        .map_err(internal)?
                        .as_str()
                        .ok_or_else(|| internal("invalid saved continuation protocol"))?
                        .into(),
                });
                continuation = Some(saved);
            }
            _ => {}
        }
    }
    let durable_binding = if let Some(tool) = &tool {
        restore_execution_bindings(&records, tool, &mut restored).await?
    } else {
        None
    };
    if continuation_binding.is_none() {
        continuation_binding = durable_binding;
    }
    if continuation_binding.is_none() {
        continuation_binding = recovered
            .receipts
            .last()
            .map(|record| record.binding.clone());
    }
    if has_native && continuation_binding.is_none() {
        return Err(internal(
            "saved native execution has no trusted provider route binding",
        ));
    }
    let mut receipt_artifacts = Vec::new();
    for record in &recovered.receipts {
        if record.stage == NativeReceiptStage::CannotResume {
            continue;
        }
        let stored = load_receipt(&record.receipt).await?;
        validate_receipt_row(&stored, record, session_id)?;
        restore_receipt_work(
            &stored,
            record,
            &records,
            tool.as_ref().expect("native tool checked"),
            &mut restored,
        )?;
        receipt_artifacts.push((record.clone(), stored));
    }
    for record in records.values().filter(|record| {
        matches!(
            record.stage,
            ToolExecutionStage::OutputPrepared | ToolExecutionStage::OutputPublished
        )
    }) {
        if !restored.contains_key(&member_id(&record.identity)) {
            return Err(internal(
                "saved native output has no recoverable provider call binding",
            ));
        }
    }
    let mut state = RoundState {
        session_id: Some(session_id),
        continuation,
        continuation_binding,
        ..Default::default()
    };
    state.pending_receipts = recovered
        .receipts
        .iter()
        .filter(|record| record.stage != NativeReceiptStage::ResponseReceived)
        .cloned()
        .collect();
    // Install work before publishing saved outputs; the ordinary result publisher
    // resolves those identities without dispatching or re-running any output hook.
    *orch.computer_runtime.work.lock().await = restored.clone();
    *orch.computer_runtime.state.lock().await = state;

    let completed: HashSet<_> = recovered
        .receipts
        .iter()
        .filter(|record| record.stage == NativeReceiptStage::ResponseReceived)
        .flat_map(|record| record.execution_ids.iter().cloned())
        .collect();
    for record in records.values() {
        if completed.contains(&record.execution_id()) {
            continue;
        }
        if !matches!(
            record.stage,
            ToolExecutionStage::OutputPrepared | ToolExecutionStage::OutputPublished
        ) {
            continue;
        }
        if compacted_abandoned_output(record, &records, &history) {
            continue;
        }
        let output = record
            .output
            .as_ref()
            .ok_or_else(|| internal("published native output has no saved artifact"))?;
        let stored = load_output(output).await?;
        let expected = member_id(&record.identity);
        if stored.row.session_id != session_id.as_uuid().to_string()
            || blocks(&stored.message)
                .iter()
                .filter(|block| {
                    matches!(block,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &expected)
                })
                .count()
                != 1
        {
            return Err(internal(
                "saved final output row has an invalid execution association",
            ));
        }
        if record.stage == ToolExecutionStage::OutputPrepared {
            if !publish_native_result(orch, &stored.message).await? {
                return Err(internal(
                    "unpublished native output has no restored binding",
                ));
            }
        }
        restore_message(orch, stored.message).await;
    }

    validate_received_boundaries(&history, &receipt_artifacts)?;
    let mut acknowledged = BTreeSet::new();
    for (record, stored) in receipt_artifacts {
        match record.stage {
            NativeReceiptStage::Prepared | NativeReceiptStage::NotSubmitted => {
                persist_stored_row(orch, &stored.row, &format!("{}:history", record.receipt_id))
                    .await?;
                restore_message(orch, stored.message).await;
            }
            NativeReceiptStage::ResponseReceived => {
                acknowledged.extend(
                    record
                        .execution_ids
                        .iter()
                        .filter_map(|id| records.get(id))
                        .map(|record| record.identity.call_identity()),
                );
            }
            _ => {}
        }
    }
    if let Some(blocker) = recovered.receipts.iter().find(|record| {
        matches!(
            record.stage,
            NativeReceiptStage::Submitted
                | NativeReceiptStage::SubmissionUnknown
                | NativeReceiptStage::CannotResume
        )
    }) {
        let reason = if blocker.stage == NativeReceiptStage::CannotResume {
            "final hook-visible computer output cannot resume the native provider call"
        } else {
            "native provider submission outcome is unknown; automatic resubmission is refused"
        };
        orch.computer_runtime.state.lock().await.fallback_reason = Some(reason.into());
        return Err(internal(reason));
    }

    let receipted: HashSet<_> = recovered
        .receipts
        .iter()
        .flat_map(|record| record.execution_ids.iter().cloned())
        .collect();
    let mut abandoned = BTreeSet::new();
    let mut facts = Vec::new();
    for (id, member) in &restored {
        if acknowledged.contains(&member.identity.call_identity()) {
            continue;
        }
        let execution_id = member.identity.execution_id();
        let record = records.get(&execution_id);
        if record.is_none_or(needs_fresh_observation) {
            abandoned.insert(member.identity.call_identity());
            facts.push(format!(
                "{}: {}",
                id,
                match record {
                    None => "input was not durably started",
                    Some(record)
                        if matches!(
                            record.stage,
                            ToolExecutionStage::Started | ToolExecutionStage::OutcomeUnknown
                        ) =>
                        "input outcome is unknown",
                    Some(record) => match record.outcome {
                        Some(ToolExecutionOutcome::Succeeded) =>
                            "input succeeded; final model-visible output is unavailable",
                        Some(ToolExecutionOutcome::Failed) =>
                            "input failed; final model-visible output is unavailable",
                        Some(ToolExecutionOutcome::Denied) => "input was denied",
                        Some(ToolExecutionOutcome::Cancelled) => "input was cancelled",
                        Some(ToolExecutionOutcome::Skipped) => "input was skipped",
                        _ => "input outcome is unknown",
                    },
                }
            ));
        }
    }
    // Even if a binding marker was lost, authoritative Started facts cannot
    // disappear or be replayed as a newly created provider call.
    for record in records
        .values()
        .filter(|record| needs_fresh_observation(record))
    {
        abandoned.insert(record.identity.call_identity());
        facts.push(format!(
            "{}: {:?}; outcome {:?}",
            record.execution_id(),
            record.stage,
            record.outcome
        ));
    }
    if !abandoned.is_empty() {
        facts.sort();
        facts.dedup();
        let reason = format!("Recovered computer execution facts: {}. Do not repeat these inputs. Obtain a fresh observation through the same provider's computer function before continuing.", facts.join("; "));
        let binding = orch
            .computer_runtime
            .state
            .lock()
            .await
            .continuation_binding
            .clone();
        let provider = abandonment_provider(&restored, &abandoned, binding.as_ref())?;
        abandon_calls(orch, session_id, provider, abandoned.clone(), &reason).await?;
    }
    let mut ready = restored
        .iter()
        .filter(|(_, member)| {
            !abandoned.contains(&member.identity.call_identity())
                && !receipted.contains(&member.identity.execution_id())
                && records
                    .get(&member.identity.execution_id())
                    .is_some_and(|record| !needs_fresh_observation(record))
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ready.sort_by_key(ToString::to_string);
    if !ready.is_empty() {
        prepare_receipts(orch, &ready).await?;
    }
    Ok(())
}
