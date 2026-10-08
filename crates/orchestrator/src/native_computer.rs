//! Provider-native computer calls use the registered Tool and ordinary dispatcher.

use crate::conversation::ConversationOrchestrator;
use crate::error::OrchestratorError;
use hooks::attachment::HookPublicationGuard;
use lingxi_core::host::{
    DurableToolOutput, NativeContinuationBinding, NativeReceiptRecord, NativeReceiptStage,
    ToolExecutionIdentity, ToolExecutionOutcome, ToolExecutionRecord, ToolExecutionStage,
};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use lingxi_llm_client::protocol::{self as wire, computer::*};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tool_api::{Tool, ToolCallResult, ToolError, ToolUseContext};

mod recovery;

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredOutput {
    message: ConversationMessage,
    row: session::jsonl::JsonlMessage,
    utf16_overrides: session::jsonl::exact_json::Utf16Overrides,
}

impl StoredOutput {
    fn new(message: ConversationMessage, row: session::jsonl::JsonlMessage) -> Self {
        let utf16_overrides = session::jsonl::exact_json::message_utf16_overrides(&row);
        Self {
            message,
            row,
            utf16_overrides,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredReceipt {
    block: wire::ContentBlock,
    message: ConversationMessage,
    row: session::jsonl::JsonlMessage,
    utf16_overrides: session::jsonl::exact_json::Utf16Overrides,
    call: NativeComputerCall,
    frame: ComputerFrame,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredExecutionBinding {
    call: NativeComputerCall,
    frame: ComputerFrame,
    binding: NativeContinuationBinding,
    provider_response_id: String,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptAcknowledgement {
    receipt_id: String,
    submission_attempt: u32,
    execution_ids: Vec<String>,
    binding: NativeContinuationBinding,
}

impl ReceiptAcknowledgement {
    fn from_record(record: &NativeReceiptRecord) -> Self {
        Self {
            receipt_id: record.receipt_id.clone(),
            submission_attempt: record.submission_attempt,
            execution_ids: record.execution_ids.clone(),
            binding: record.binding.clone(),
        }
    }
}

/// Enable native projection only for a model/profile with recorded live acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedComputerProfile {
    pub model: String,
    pub profile: Option<String>,
    pub provider: NativeComputerProvider,
    pub evidence: String,
}

#[derive(Clone)]
struct Binding {
    tool: Arc<dyn Tool>,
    provider: NativeComputerProvider,
    frame: ComputerFrame,
    model: String,
    profile: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Work {
    pub(crate) call: Arc<NativeComputerCall>,
    pub(crate) index: usize,
    pub(crate) identity: ToolExecutionIdentity,
    pub(crate) frame: ComputerFrame,
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) binding: NativeContinuationBinding,
}

#[derive(Default)]
struct RoundState {
    session_id: Option<lingxi_core::types::SessionId>,
    next_native: bool,
    active: Option<Binding>,
    continuation: Option<wire::ContinuationRef>,
    continuation_binding: Option<NativeContinuationBinding>,
    pending_receipts: Vec<NativeReceiptRecord>,
    fallback_reason: Option<String>,
}

#[derive(Default)]
pub(crate) struct ComputerRuntime {
    verified: RwLock<Vec<VerifiedComputerProfile>>,
    state: Mutex<RoundState>,
    work: Mutex<HashMap<ToolUseId, Work>>,
}

tokio::task_local! {
    static TURN_CANCEL: Option<CancellationToken>;
    static NATIVE_WORK: Work;
    static BUFFER_STREAM: bool;
}

pub(crate) async fn scope_turn_cancel<F: Future>(
    cancel: CancellationToken,
    future: F,
) -> F::Output {
    TURN_CANCEL.scope(Some(cancel), Box::pin(future)).await
}

pub(crate) fn current_turn_cancel() -> Option<CancellationToken> {
    TURN_CANCEL.try_with(Clone::clone).ok().flatten()
}

pub(crate) async fn scope_buffered_stream<F: Future>(future: F) -> F::Output {
    BUFFER_STREAM.scope(true, Box::pin(future)).await
}

pub(crate) async fn call_model(
    orch: &ConversationOrchestrator,
    request: llm_runtime::MessagesCreateRequest,
) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
    if BUFFER_STREAM.try_with(|enabled| *enabled).unwrap_or(false) {
        orch.api.messages_create_buffered_stream(request).await
    } else {
        orch.api
            .messages_create(crate::OrchestratorApiRequest::Main(request))
            .await
    }
}

pub(crate) async fn uses_buffered_protocol(orch: &ConversationOrchestrator) -> bool {
    let state = orch.computer_runtime.state.lock().await;
    if !state.pending_receipts.is_empty() {
        return true;
    }
    drop(state);
    let (model, profile) = {
        let session = orch.session.lock().await;
        (session.model.clone(), session.model_profile.clone())
    };
    let verified = orch
        .computer_runtime
        .verified
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|p| p.model == model && p.profile == profile);
    verified
        && orch
            .filtered_available_tools()
            .await
            .iter()
            .any(|t| !t.is_mcp() && t.native_computer_capabilities().is_some())
}

pub(crate) async fn cancel_before_effects<F: Future<Output = Result<T, OrchestratorError>>, T>(
    future: F,
) -> Result<T, OrchestratorError> {
    match current_turn_cancel() {
        Some(cancel) => tokio::select! {
            biased;
            () = cancel.cancelled() => Err(OrchestratorError::Internal("model request cancelled".into())),
            result = future => result,
        },
        None => future.await,
    }
}

fn internal(error: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::Internal(format!("native computer: {error}"))
}

fn digest(value: &Value) -> Result<String, OrchestratorError> {
    let bytes = serde_json::to_vec(value).map_err(internal)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn protocol(provider: NativeComputerProvider) -> wire::ProtocolFamily {
    match provider {
        NativeComputerProvider::OpenAi => wire::ProtocolFamily::OpenAiResponses,
        NativeComputerProvider::Anthropic => wire::ProtocolFamily::AnthropicMessages,
        NativeComputerProvider::Gemini => wire::ProtocolFamily::GeminiInteractions,
    }
}

fn protocol_name(provider: NativeComputerProvider) -> String {
    serde_json::to_value(protocol(provider))
        .expect("protocol serializes")
        .as_str()
        .unwrap()
        .into()
}

impl ConversationOrchestrator {
    /// Wire the existing session coordinator's durable side-effect journal.
    #[must_use]
    pub fn with_tool_execution_journal(
        mut self,
        journal: Arc<dyn lingxi_core::host::ToolExecutionJournal>,
    ) -> Self {
        self.tool_execution_journal = Some(journal);
        self
    }

    /// Host-controlled allowlist; an empty list keeps ordinary function projection.
    #[must_use]
    pub fn with_verified_computer_profiles(self, profiles: Vec<VerifiedComputerProfile>) -> Self {
        *self
            .computer_runtime
            .verified
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = profiles
            .into_iter()
            .filter(|p| !p.evidence.trim().is_empty())
            .collect();
        self
    }

    pub async fn computer_projection_reason(&self) -> Option<String> {
        self.computer_runtime
            .state
            .lock()
            .await
            .fallback_reason
            .clone()
    }
}

/// Restore durable rows before the turn captures or renders its request history.
pub(crate) async fn recover_before_request(
    orch: &ConversationOrchestrator,
) -> Result<(), OrchestratorError> {
    let session_id = orch.session.lock().await.session_id;
    let changed_owner = orch.computer_runtime.state.lock().await.session_id != Some(session_id);
    if changed_owner {
        if orch
            .computer_runtime
            .state
            .lock()
            .await
            .session_id
            .is_some()
        {
            cleanup(orch).await?;
        }
        recovery::rehydrate(orch, session_id).await?;
    } else {
        recovery::refresh_pending(orch, session_id).await?;
    }
    Ok(())
}

pub(crate) async fn preserve_pending_compaction_tail(
    orch: &ConversationOrchestrator,
    preserved: &[ConversationMessage],
) -> Result<Vec<ConversationMessage>, OrchestratorError> {
    recovery::preserve_pending_compaction_tail(orch, preserved).await
}

/// Select from the prepared catalog without appending rows to its history snapshot.
pub(crate) async fn prepare_projection(
    orch: &ConversationOrchestrator,
    model: &str,
    profile: Option<&str>,
    tools: &[Value],
) -> Result<Option<llm_runtime::computer::ComputerRequestProjection>, OrchestratorError> {
    let available = tools
        .iter()
        .any(|t| t["name"] == "computer" && t["defer_loading"] != true);
    let tool = if available {
        orch.filtered_available_tools().await.into_iter().find(|t| {
            t.name() == "computer" && !t.is_mcp() && t.native_computer_capabilities().is_some()
        })
    } else {
        None
    };
    let provider = orch.api.native_computer_provider(model, profile);
    let retained_continuations = orch
        .session
        .lock()
        .await
        .history
        .iter()
        .flat_map(|message| match message {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content.as_slice(),
            ConversationMessage::System { .. } => &[],
        })
        .filter_map(|block| match block {
            ContentBlock::ProviderContent { value, .. }
                if value["type"] == "lingxi_computer_continuation" =>
            {
                Some(value["continuation"].clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut state = orch.computer_runtime.state.lock().await;
    if state.pending_receipts.is_empty()
        && state.continuation.as_ref().is_some_and(|reference| {
            !retained_continuations
                .contains(&serde_json::to_value(reference).expect("continuation serializes"))
        })
    {
        state.continuation = None;
        state.continuation_binding = None;
        state.next_native = false;
    }
    // A submitted receipt is never recreated after an uncertain provider response.
    if let Some(journal) = &orch.tool_execution_journal {
        for receipt in &mut state.pending_receipts {
            if let Some(stored) = journal
                .receipt(&receipt.receipt_id)
                .await
                .map_err(internal)?
            {
                *receipt = stored;
            }
            if matches!(
                receipt.stage,
                NativeReceiptStage::Submitted
                    | NativeReceiptStage::SubmissionUnknown
                    | NativeReceiptStage::CannotResume
            ) {
                return Err(internal(
                    "prior receipt submission has unknown outcome; observe before continuing",
                ));
            }
        }
    }
    state.active = None;
    let Some(provider) = provider else {
        state.fallback_reason = Some("selected protocol has no native desktop projection".into());
        if !state.pending_receipts.is_empty() || state.continuation.is_some() {
            return Err(internal("computer continuation cannot change provider"));
        }
        return Ok(None);
    };
    if state
        .continuation
        .as_ref()
        .is_some_and(|c| c.protocol != protocol(provider))
    {
        return Err(internal("computer continuation cannot change protocol"));
    }
    let verified = orch
        .computer_runtime
        .verified
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|p| p.model == model && p.profile.as_deref() == profile && p.provider == provider);
    let mut native = None;
    if state.next_native && verified {
        if let Some(tool) = tool {
            let ctx = crate::turn_loop::streaming_tool_context_base(orch, Vec::new()).await;
            let frame = tool.native_computer_frame(&ctx).await.map_err(internal)?;
            let durable = orch.tool_execution_journal.is_some()
                && orch.config_home.is_some()
                && orch
                    .transcript
                    .jsonl_writer
                    .as_ref()
                    .is_some_and(|w| w.durable_transcript_enabled());
            if let Some(frame) = frame.filter(|_| durable) {
                let capabilities = tool
                    .native_computer_capabilities()
                    .expect("filtered capability");
                // Use the same declaration conversion as the actual request:
                // native namespaces must coexist with every ordinary tool.
                let declarations = llm_runtime::convert::to_tool_declarations(
                    tools
                        .iter()
                        .filter(|t| t["name"] != "computer")
                        .cloned()
                        .collect(),
                )
                .map_err(internal)?;
                let (mut probe, _) = llm_runtime::convert::history_input(
                    model,
                    &[],
                    &[],
                    &declarations,
                    protocol(provider),
                )
                .map_err(internal)?;
                match declare_computer_tool(&mut probe, provider, &capabilities, &frame) {
                    Ok(()) => {
                        native = Some(llm_runtime::computer::ComputerNativeDeclaration {
                            provider,
                            capabilities,
                            frame: frame.clone(),
                        });
                        state.active = Some(Binding {
                            tool,
                            provider,
                            frame,
                            model: model.into(),
                            profile: profile.map(str::to_owned),
                        });
                        state.fallback_reason = None;
                    }
                    Err(error) => {
                        state.fallback_reason =
                            Some(format!("native declaration unavailable: {error}"))
                    }
                }
            } else {
                state.fallback_reason = Some(
                    "authorized current frame or durable execution services unavailable".into(),
                );
            }
        } else {
            state.fallback_reason =
                Some("builtin computer is outside the effective catalog".into());
        }
    } else {
        state.fallback_reason = Some(
            if verified {
                "function management round"
            } else {
                "model/profile has no live native acceptance record"
            }
            .into(),
        );
    }
    if !available && state.pending_receipts.is_empty() && state.continuation.is_none() {
        return Ok(None);
    }
    let submission = orch
        .tool_execution_journal
        .as_ref()
        .filter(|_| !state.pending_receipts.is_empty())
        .map(|journal| {
            Arc::new(llm_runtime::computer::ComputerReceiptSubmission {
                journal: journal.clone(),
                receipts: state.pending_receipts.clone(),
            })
        });
    Ok(Some(llm_runtime::computer::ComputerRequestProjection {
        native,
        continuation: state.continuation.clone(),
        binding: state
            .pending_receipts
            .first()
            .map(|r| r.binding.clone())
            .or_else(|| state.continuation_binding.clone()),
        submission,
    }))
}

pub(crate) async fn note_computer_result(
    orch: &ConversationOrchestrator,
    tool: &Arc<dyn Tool>,
    ctx: &ToolUseContext,
    success: bool,
) {
    if tool.is_mcp() || tool.native_computer_capabilities().is_none() {
        return;
    }
    // Readiness is supplied by the tool's actual action outcome, grants and frame.
    let ready = success
        && tool
            .native_computer_frame(ctx)
            .await
            .ok()
            .flatten()
            .is_some();
    let mut state = orch.computer_runtime.state.lock().await;
    if state.active.is_none() {
        state.next_native = ready;
    }
}

/// Fully decode/lower a response before publishing or executing any member.
pub(crate) async fn bind_response(
    orch: &ConversationOrchestrator,
    response: &llm_runtime::HistoryResponse,
    mut blocks: Vec<ContentBlock>,
) -> Result<Vec<ContentBlock>, OrchestratorError> {
    let binding = orch.computer_runtime.state.lock().await.active.clone();
    let continuation = llm_runtime::computer::response_continuation(response).map_err(internal)?;
    let acknowledged = receipt_acknowledgements(orch).await?;
    if let Some(binding) = binding {
        let (model, profile) = {
            let s = orch.session.lock().await;
            (s.model.clone(), s.model_profile.clone())
        };
        if model != binding.model || profile != binding.profile {
            return Err(internal(
                "model/profile changed while native response was in flight",
            ));
        }
        let current = orch
            .filtered_available_tools()
            .await
            .into_iter()
            .any(|t| Arc::ptr_eq(&t, &binding.tool));
        if !current {
            return Err(internal("native computer is no longer in scope"));
        }
        let canonical =
            llm_runtime::computer::canonical_response_content(response, protocol(binding.provider))
                .map_err(internal)?;
        if canonical.iter().any(|b| matches!(b, wire::ContentBlock::ToolUse { name, toolset_name, .. } if name == "computer" && toolset_name.is_none())) {
            return Err(internal("function computer call was not declared in this native round"));
        }
        let calls = llm_runtime::computer::decode_computer_calls(
            response,
            binding.provider,
            &binding.frame,
        )
        .map_err(internal)?;
        let call_ids: std::collections::HashSet<_> =
            calls.iter().map(|c| c.context.call_id.clone()).collect();
        let session_id = orch.session.lock().await.session_id;
        let execution_binding = llm_runtime::computer::response_execution_binding(response)
            .ok_or_else(|| internal("native response has no trusted route/account binding"))?;
        let mut pending = Vec::new();
        let mut replacements = HashMap::new();
        for call in calls {
            let call = Arc::new(call);
            let mut replacement = Vec::new();
            let originals: Vec<_> = canonical
                .iter()
                .filter(|b| computer_call_id(b) == Some(call.context.call_id.as_str()))
                .cloned()
                .collect();
            if originals.is_empty() {
                return Err(internal("native call has no original wire block"));
            }
            let mut ids = Vec::new();
            for (index, operation) in call.operations.iter().enumerate() {
                let input = binding
                    .tool
                    .lower_computer_operation(operation, &binding.frame)
                    .map_err(internal)?;
                let identity = ToolExecutionIdentity {
                    session_id,
                    provider_response_id: response.id.clone(),
                    provider_call_id: call.context.call_id.clone(),
                    member_index: u32::try_from(index).map_err(internal)?,
                };
                let id = ToolUseId::from(format!(
                    "native_computer_{}",
                    identity
                        .execution_id()
                        .trim_start_matches("tool-execution:")
                ));
                ids.push(id.to_string());
                replacement.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: binding.tool.name().into(),
                    input,
                    provider_id: Some(id.to_string()),
                });
                pending.push((
                    id,
                    Work {
                        call: call.clone(),
                        index,
                        identity,
                        frame: binding.frame.clone(),
                        tool: binding.tool.clone(),
                        binding: execution_binding.clone(),
                    },
                ));
            }
            let call_id = call.context.call_id.clone();
            replacement.push(ContentBlock::ProviderContent {
                protocol: protocol_name(binding.provider),
                value: json!({"type":"lingxi_computer_binding","call":call.as_ref(),"tool_use_ids":ids,"original_blocks":originals,"frame":binding.frame,"binding":execution_binding,"provider_response_id":response.id}),
            });
            replacements.insert(call_id, replacement);
        }
        let originals = std::mem::take(&mut blocks);
        for block in originals {
            if let Some(id) = host_native_call_id(&block).filter(|id| call_ids.contains(id)) {
                if let Some(replacement) = replacements.remove(&id) {
                    blocks.extend(replacement);
                }
            } else {
                blocks.push(block);
            }
        }
        if !replacements.is_empty() {
            return Err(internal(
                "native original block order was not preserved by response projection",
            ));
        }
        orch.computer_runtime.work.lock().await.extend(pending);
        orch.computer_runtime.state.lock().await.next_native = false;
    }
    if orch.computer_runtime.state.lock().await.active.is_none() {
        let (model, profile) = {
            let session = orch.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        if let Some(provider) = orch
            .api
            .native_computer_provider(&model, profile.as_deref())
        {
            let canonical =
                llm_runtime::computer::canonical_response_content(response, protocol(provider))
                    .map_err(internal)?;
            if canonical
                .iter()
                .any(|block| computer_call_id(block).is_some())
            {
                return Err(internal(
                    "provider returned native computer work without a scoped declaration",
                ));
            }
        }
    }
    if let Some(continuation) = continuation {
        blocks.push(ContentBlock::ProviderContent {
            protocol: serde_json::to_value(continuation.protocol)
                .map_err(internal)?
                .as_str()
                .unwrap()
                .into(),
            value: json!({"type":"lingxi_computer_continuation","continuation":continuation}),
        });
        let mut state = orch.computer_runtime.state.lock().await;
        state.continuation_binding = llm_runtime::computer::response_execution_binding(response)
            .or_else(|| {
                Some(NativeContinuationBinding {
                    account: continuation.account_scope.clone(),
                    profile: continuation.profile_name.clone(),
                    model: continuation.request_model.clone(),
                    endpoint: continuation.endpoint_fingerprint.clone(),
                    protocol: serde_json::to_value(continuation.protocol)
                        .expect("protocol serializes")
                        .as_str()
                        .unwrap()
                        .into(),
                })
            });
        state.continuation = Some(continuation);
    }
    if !acknowledged.is_empty() {
        let (model, profile) = {
            let session = orch.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        let provider = orch
            .api
            .native_computer_provider(&model, profile.as_deref())
            .or_else(|| binding_provider(&blocks));
        if let Some(provider) = provider {
            blocks.push(ContentBlock::ProviderContent {
                protocol: protocol_name(provider),
                value: json!({"type":"lingxi_computer_receipt_ack","call_ids":acknowledged,"provider_response_id":response.id,"receipts":receipt_acknowledgement_bindings(orch).await?}),
            });
        }
    }
    Ok(blocks)
}

fn host_native_call_id(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::ToolUse {
            id, provider_id, ..
        } => Some(provider_id.clone().unwrap_or_else(|| id.to_string())),
        ContentBlock::ProviderContent { value, .. }
            if matches!(
                value["type"].as_str(),
                Some("lingxi_native_content" | "lingxi_replay_metadata")
            ) =>
        {
            serde_json::from_value::<wire::ContentBlock>(value["block"].clone())
                .ok()
                .and_then(|block| computer_call_id(&block).map(str::to_owned))
        }
        _ => None,
    }
}

fn binding_provider(blocks: &[ContentBlock]) -> Option<NativeComputerProvider> {
    blocks.iter().find_map(|b| match b {
        ContentBlock::ProviderContent { protocol, .. } => match protocol.as_str() {
            "anthropic_messages" => Some(NativeComputerProvider::Anthropic),
            "open_ai_responses" => Some(NativeComputerProvider::OpenAi),
            "gemini_interactions" => Some(NativeComputerProvider::Gemini),
            _ => None,
        },
        _ => None,
    })
}

/// Called by the ordinary dispatcher after hooks, approval edits and validation.
pub(crate) async fn before_execution(
    orch: &ConversationOrchestrator,
    id: &ToolUseId,
    tool: &Arc<dyn Tool>,
    input: &Value,
    ctx: &ToolUseContext,
) -> Result<Option<ToolExecutionRecord>, ToolError> {
    let Some(work) = NATIVE_WORK.try_with(Clone::clone).ok() else {
        if id.as_str().starts_with("native_computer_") {
            return Err(ToolError::InvalidInput(
                "saved native member requires restored journal admission; ordinary replay refused"
                    .into(),
            ));
        }
        return Ok(None);
    };
    if !orch
        .filtered_available_tools()
        .await
        .iter()
        .any(|candidate| Arc::ptr_eq(candidate, tool))
    {
        return Err(ToolError::PermissionDenied(
            "native computer left the effective tool catalog before execution".into(),
        ));
    }
    if let Some(active) = orch.computer_runtime.state.lock().await.active.as_ref() {
        let session = orch.session.lock().await;
        if session.model != active.model || session.model_profile != active.profile {
            return Err(ToolError::InvalidInput(
                "native model/profile changed before execution".into(),
            ));
        }
    }
    if !Arc::ptr_eq(tool, &work.tool) || tool.is_mcp() {
        return Err(ToolError::InvalidInput(
            "native call cannot replace its registered executor".into(),
        ));
    }
    if ctx.tool_use_id.as_ref() != Some(id)
        || ctx
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(ToolError::Aborted);
    }
    if input["geometry_version"] != work.frame.geometry_version {
        return Err(ToolError::InvalidInput(
            "final native input changed or removed observation geometry".into(),
        ));
    }
    if matches!(
        work.call.operations[work.index],
        ComputerOperation::Screenshot | ComputerOperation::Zoom { .. }
    ) && !matches!(input["action"].as_str(), Some("screenshot" | "zoom"))
    {
        return Err(ToolError::InvalidInput(
            "native observation requires an image-producing final action".into(),
        ));
    }
    let mut acknowledged_safety_checks = Vec::new();
    if !work.call.context.pending_safety_checks.is_empty() && work.index == 0 {
        if !orch.config.interactive_permissions {
            return Err(ToolError::InteractionRequired(
                "provider computer safety confirmation".into(),
            ));
        }
        let check = lingxi_core::host::permission_gate::PermissionCheckContext {
            input_projection: Some(
                ctx.projected_input(input)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?,
            ),
            tool_use_id: Some(id.to_string()),
            decision_reason_type: Some("safetyCheck".into()),
            decision_reason: Some(
                serde_json::to_string(&work.call.context.pending_safety_checks)
                    .map_err(|e| ToolError::Internal(e.to_string()))?,
            ),
            ..Default::default()
        };
        let approval = orch.perms.ask_via_transport(tool.name(), input, &check);
        let outcome = match &ctx.cancel {
            Some(cancel) => tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ToolError::Aborted),
                outcome = approval => outcome,
            },
            None => approval.await,
        };
        match outcome {
            lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input, ..
            } if updated_input
                .as_ref()
                .is_none_or(|v| Some(v) == check.input_projection.as_ref()) =>
            {
                acknowledged_safety_checks = work.call.context.pending_safety_checks.clone();
            }
            lingxi_core::host::permission_gate::PermissionOutcome::Allow { .. } => {
                return Err(ToolError::InvalidInput(
                    "safety confirmation changed the already reviewed final input".into(),
                ));
            }
            _ => {
                return Err(ToolError::PermissionDenied(
                    "provider computer safety check was not acknowledged".into(),
                ));
            }
        }
    }
    let journal = orch
        .tool_execution_journal
        .as_ref()
        .ok_or_else(|| ToolError::Internal("native execution journal unavailable".into()))?;
    let record = ToolExecutionRecord {
        identity: work.identity.clone(),
        tool_name: tool.name().into(),
        input_digest: digest(input).map_err(|e| ToolError::Internal(e.to_string()))?,
        recovery_binding: store_execution_binding(orch, &work)
            .await
            .map_err(|e| ToolError::Io(e.to_string()))?,
        acknowledged_safety_checks,
        stage: ToolExecutionStage::Started,
        outcome: None,
        output: None,
    };
    let ack = journal
        .record_execution(record.clone())
        .await
        .map_err(|e| ToolError::Io(e.to_string()))?;
    if ack.duplicate {
        return Err(ToolError::InvalidInput(
            "native input already started; automatic replay refused".into(),
        ));
    }
    Ok(Some(record))
}

pub(crate) fn after_execution<'a>(
    orch: &'a ConversationOrchestrator,
    record: Option<ToolExecutionRecord>,
    result: &Result<ToolCallResult, ToolError>,
) -> impl Future<Output = Result<(), ToolError>> + Send + 'a {
    let outcome = match result {
        Ok(result) if !result.is_error => ToolExecutionOutcome::Succeeded,
        Ok(_) => ToolExecutionOutcome::Failed,
        Err(ToolError::Aborted) => ToolExecutionOutcome::Cancelled,
        Err(ToolError::PermissionDenied(_) | ToolError::InteractionRequired(_)) => {
            ToolExecutionOutcome::Denied
        }
        Err(_) => ToolExecutionOutcome::Failed,
    };
    async move {
        let Some(mut record) = record else {
            return Ok(());
        };
        record.stage = ToolExecutionStage::Terminal;
        record.outcome = Some(outcome);
        orch.tool_execution_journal
            .as_ref()
            .ok_or_else(|| ToolError::Internal("native journal disappeared".into()))?
            .record_execution(record)
            .await
            .map_err(|e| ToolError::Io(e.to_string()))?;
        Ok(())
    }
}

fn result_error(id: &ToolUseId, message: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.clone(),
        content: message.into(),
        is_error: Some(true),
        provider_tool_use_id: Some(id.to_string()),
        content_blocks: None,
    }
}

/// Only native sequences are ordered here; ordinary batches retain their scheduler.
pub(crate) async fn dispatch_tools(
    orch: &ConversationOrchestrator,
    uses: &[(ToolUseId, String, Value, Option<String>)],
    assistant_id: MessageId,
    facts: crate::turn_loop::ToolUseDispatchFacts,
    fence: crate::autonomous_tool_scheduler::ToolDispatchPublicationFence,
) -> Result<crate::turn_loop::DeferredToolDispatch, OrchestratorError> {
    let work = orch.computer_runtime.work.lock().await.clone();
    if !uses.iter().any(|(id, ..)| work.contains_key(id)) {
        return crate::turn_loop::tool_dispatch::dispatch_tool_uses_tracked_deferred_with_facts(
            orch,
            uses,
            current_turn_cancel(),
            Some(assistant_id),
            Some(facts),
            None,
            None,
            None,
            Some(Arc::new(fence)),
        )
        .await;
    }
    let mut ctx =
        crate::turn_loop::streaming_tool_context_base(orch, facts.query_history.clone()).await;
    ctx.cancel = current_turn_cancel();
    let tool = uses
        .iter()
        .find_map(|(id, ..)| work.get(id))
        .expect("native member")
        .tool
        .clone();
    tool.begin_computer_sequence(&ctx).await.map_err(internal)?;
    let result = async {
        let mut aggregate = crate::turn_loop::DeferredToolDispatch::default();
        let mut stop = false;
        for call in uses {
            let Some(member) = work.get(&call.0).cloned() else {
                let result = crate::turn_loop::tool_dispatch::dispatch_tool_uses_tracked_deferred_with_facts(
                    orch, std::slice::from_ref(call), current_turn_cancel(), Some(assistant_id), Some(facts.clone()), None, None, None, Some(Arc::new(fence.clone())),
                ).await?;
                merge_dispatch(&mut aggregate, result);
                continue;
            };
            let journal = orch.tool_execution_journal.as_ref().ok_or_else(|| internal("missing native journal"))?;
            if let Some(record) = journal.execution(&member.identity.execution_id()).await.map_err(internal)? {
                let blocks = match record.output {
                    Some(output) => match load_output(&output).await?.message {
                        ConversationMessage::User { content, .. } => content,
                        _ => return Err(internal("saved output is not a tool result row")),
                    },
                    None => vec![result_error(&call.0, "Native input has already started; outcome requires observation. Input was not replayed.")],
                };
                stop |= record.outcome != Some(ToolExecutionOutcome::Succeeded);
                aggregate.results.extend(blocks);
                continue;
            }
            if stop || ctx.cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                let outcome = if stop { ToolExecutionOutcome::Skipped } else { ToolExecutionOutcome::Cancelled };
                record_no_input(orch, &member, &call.2, outcome).await?;
                aggregate.results.push(result_error(&call.0, "Skipped: earlier native action failed, was denied, or was cancelled."));
                continue;
            }
            let result = NATIVE_WORK.scope(member.clone(), Box::pin(crate::turn_loop::dispatch_streaming_tool_use_owned(
                orch, call, ctx.cancel.clone(), assistant_id, facts.clone(), member.tool.clone(), Some(ctx.clone()), None, fence.clone(),
            ))).await?;
            let failed = result.results.iter().any(|b| matches!(b, ContentBlock::ToolResult { is_error: Some(true), .. }));
            let mut execution = journal.execution(&member.identity.execution_id()).await.map_err(internal)?;
            if execution.is_none() {
                let outcome = if ctx.cancel.as_ref().is_some_and(CancellationToken::is_cancelled)
                    || result.publications.iter().any(|p| p.denial_kind.as_deref() == Some("cancelled")) {
                    ToolExecutionOutcome::Cancelled
                } else if result.publications.iter().any(|p| p.permission_denial.is_some() || p.denial_kind.is_some()) {
                    ToolExecutionOutcome::Denied
                } else { ToolExecutionOutcome::Failed };
                record_no_input(orch, &member, &call.2, outcome).await?;
                execution = journal.execution(&member.identity.execution_id()).await.map_err(internal)?;
            }
            stop |= execution.is_none_or(|record| record.outcome != Some(ToolExecutionOutcome::Succeeded))
                || failed || result.prevent_continuation;
            merge_dispatch(&mut aggregate, result);
        }
        Ok(aggregate)
    }.await;
    let finish = tool.end_computer_sequence(&ctx).await;
    if result.is_err()
        || ctx
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    {
        tool.cleanup_computer_inputs(&ctx).await.map_err(internal)?;
    }
    finish.map_err(internal)?;
    result
}

fn merge_dispatch(
    target: &mut crate::turn_loop::DeferredToolDispatch,
    mut source: crate::turn_loop::DeferredToolDispatch,
) {
    target.results.append(&mut source.results);
    target.publications.append(&mut source.publications);
    target.prevent_continuation |= source.prevent_continuation;
    target
        .injected_messages
        .append(&mut source.injected_messages);
    target
        .context_modifiers
        .append(&mut source.context_modifiers);
    target
        .post_tool_batch_calls
        .append(&mut source.post_tool_batch_calls);
}

async fn record_no_input(
    orch: &ConversationOrchestrator,
    work: &Work,
    input: &Value,
    outcome: ToolExecutionOutcome,
) -> Result<(), OrchestratorError> {
    let journal = orch
        .tool_execution_journal
        .as_ref()
        .ok_or_else(|| internal("missing native journal"))?;
    let mut record = ToolExecutionRecord {
        identity: work.identity.clone(),
        tool_name: work.tool.name().into(),
        input_digest: digest(input)?,
        recovery_binding: store_execution_binding(orch, work).await?,
        acknowledged_safety_checks: vec![],
        stage: ToolExecutionStage::Started,
        outcome: None,
        output: None,
    };
    let ack = journal
        .record_execution(record.clone())
        .await
        .map_err(internal)?;
    if ack.duplicate {
        return Err(internal("duplicate unexecuted native member"));
    }
    record.stage = ToolExecutionStage::Terminal;
    record.outcome = Some(outcome);
    journal.record_execution(record).await.map_err(internal)?;
    Ok(())
}

/// Notify the real tool only with final, model-visible output after all hooks/Mods.
pub(crate) async fn final_model_result(
    orch: &ConversationOrchestrator,
    blocks: &[ContentBlock],
) -> Result<(), OrchestratorError> {
    for block in blocks {
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            content_blocks,
            is_error,
            ..
        } = block
        else {
            continue;
        };
        let work = orch
            .computer_runtime
            .work
            .lock()
            .await
            .get(tool_use_id)
            .cloned();
        let tool = match work {
            Some(work) => Some(work.tool),
            None => {
                let name = orch
                    .session
                    .lock()
                    .await
                    .history
                    .iter()
                    .rev()
                    .find_map(|message| {
                        if let ConversationMessage::Assistant { content, .. } = message {
                            content.iter().find_map(|block| match block {
                                ContentBlock::ToolUse { id, name, .. } if id == tool_use_id => {
                                    Some(name.clone())
                                }
                                _ => None,
                            })
                        } else {
                            None
                        }
                    });
                match name {
                    Some(name) => orch
                        .filtered_available_tools()
                        .await
                        .into_iter()
                        .find(|t| t.name() == name && !t.is_mcp()),
                    None => None,
                }
            }
        };
        let Some(tool) = tool.filter(|t| t.native_computer_capabilities().is_some()) else {
            continue;
        };
        let mut ctx = crate::turn_loop::streaming_tool_context_base(orch, Vec::new()).await;
        ctx.tool_use_id = Some(tool_use_id.clone());
        let mut visible = content_blocks.clone().unwrap_or_default();
        for sibling in blocks {
            if let ContentBlock::Image { .. } = sibling {
                visible.push(serde_json::to_value(sibling).map_err(internal)?);
            }
        }
        tool.computer_model_output(&ctx, content, Some(&visible))
            .await
            .map_err(internal)?;
        note_computer_result(orch, &tool, &ctx, *is_error != Some(true)).await;
    }
    Ok(())
}

/// Store large final outputs in the existing tool-results media directory, then fsync.
async fn store_payload(
    orch: &ConversationOrchestrator,
    id: &str,
    payload: &Value,
) -> Result<DurableToolOutput, OrchestratorError> {
    let home = orch
        .config_home
        .as_ref()
        .ok_or_else(|| internal("native output media root unavailable"))?;
    let session_id = orch.session.lock().await.session_id;
    let dir = crate::tool_result_persistence::tool_results_dir(
        home,
        &orch.current_cwd().to_string_lossy(),
        &session_id.as_uuid().to_string(),
    );
    let hash = digest(payload)?;
    let media_id = format!("computer-{}", digest(&json!([id, hash]))?);
    let body = serde_json::to_string(payload).map_err(internal)?;
    if body.len() > 32 * 1024 * 1024 {
        return Err(internal("native final result exceeds media limit"));
    }
    let stored = crate::tool_result_persistence::persist(
        home,
        &dir,
        &media_id,
        &body,
        true,
        32 * 1024 * 1024,
    )
    .await
    .map_err(internal)?;
    let existing = tokio::fs::read(&stored.filepath).await.map_err(internal)?;
    if existing != body.as_bytes() {
        return Err(internal("native result media identity conflicts"));
    }
    tokio::fs::File::open(&stored.filepath)
        .await
        .map_err(internal)?
        .sync_all()
        .await
        .map_err(internal)?;
    tokio::fs::File::open(&dir)
        .await
        .map_err(internal)?
        .sync_all()
        .await
        .map_err(internal)?;
    let path = stored.filepath.to_string_lossy().into_owned();
    Ok(DurableToolOutput {
        digest: hash,
        payload: json!({"path":path}),
        media_refs: vec![path],
    })
}

async fn store_execution_binding(
    orch: &ConversationOrchestrator,
    work: &Work,
) -> Result<DurableToolOutput, OrchestratorError> {
    let binding = StoredExecutionBinding {
        call: work.call.as_ref().clone(),
        frame: work.frame.clone(),
        binding: work.binding.clone(),
        provider_response_id: work.identity.provider_response_id.clone(),
    };
    store_payload(
        orch,
        &format!("{}:binding", work.identity.execution_id()),
        &serde_json::to_value(binding).map_err(internal)?,
    )
    .await
}

async fn load_payload(output: &DurableToolOutput) -> Result<Value, OrchestratorError> {
    let path = output.payload["path"]
        .as_str()
        .ok_or_else(|| internal("native result media reference missing"))?;
    let meta = tokio::fs::symlink_metadata(path).await.map_err(internal)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 32 * 1024 * 1024 {
        return Err(internal("native media reference unsafe or oversized"));
    }
    let bytes = tokio::fs::read(path).await.map_err(internal)?;
    let value = serde_json::from_slice(&bytes).map_err(internal)?;
    if digest(&value)? != output.digest {
        return Err(internal("native result media digest mismatch"));
    }
    Ok(value)
}

async fn load_output(output: &DurableToolOutput) -> Result<StoredOutput, OrchestratorError> {
    let mut stored: StoredOutput =
        serde_json::from_value(load_payload(output).await?).map_err(internal)?;
    session::jsonl::exact_json::set_message_utf16_overrides(
        &mut stored.row,
        stored.utf16_overrides.clone(),
    );
    Ok(stored)
}

async fn load_receipt(output: &DurableToolOutput) -> Result<StoredReceipt, OrchestratorError> {
    let mut stored: StoredReceipt =
        serde_json::from_value(load_payload(output).await?).map_err(internal)?;
    session::jsonl::exact_json::set_message_utf16_overrides(
        &mut stored.row,
        stored.utf16_overrides.clone(),
    );
    Ok(stored)
}

/// Build the final ordinary row once, before its publication is journaled.
async fn prepare_stored_row(
    orch: &ConversationOrchestrator,
    message: &ConversationMessage,
) -> Result<session::jsonl::JsonlMessage, OrchestratorError> {
    let session_id = orch.session.lock().await.session_id;
    let mut row = orch.to_jsonl_message_with_inner_id(
        message,
        &session_id.as_uuid().to_string(),
        orch.transcript.last_jsonl_uuid.lock().await.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    if let Some(result) = orch.take_tool_use_result(message).await {
        row.extra.insert("toolUseResult".into(), result);
    }
    if let Some(kind) = orch.take_tool_denial_kind(message).await {
        row.extra.insert("toolDenialKind".into(), json!(kind));
    }
    if let Some(meta) = orch.take_tool_use_mcp_meta(message).await {
        row.extra.insert("mcpMeta".into(), meta);
    }
    if let Some(source) = orch.take_source_tool_assistant_uuid(message).await {
        row.extra
            .insert("sourceToolAssistantUUID".into(), json!(source));
    }
    Ok(row)
}

/// Fsync the exact saved row; recovery retains IDs, timestamps and metadata.
async fn persist_stored_row(
    orch: &ConversationOrchestrator,
    row: &session::jsonl::JsonlMessage,
    delivery: &str,
) -> Result<(), OrchestratorError> {
    let writer = orch
        .transcript
        .jsonl_writer
        .as_ref()
        .ok_or_else(|| internal("native transcript unavailable"))?;
    let session_id = orch.session.lock().await.session_id;
    if row.session_id != session_id.as_uuid().to_string() {
        return Err(internal("saved row belongs to another session"));
    }
    let exact = session::jsonl::exact_json::message_utf16_overrides(row);
    let (_, is_tip) = writer
        .append_json_once_durable_for_session_with_tip_exact(
            session_id,
            delivery,
            serde_json::to_value(row).map_err(internal)?,
            exact,
        )
        .await
        .map_err(internal)?;
    if is_tip && orch.session.lock().await.session_id == session_id {
        *orch.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
    }
    Ok(())
}

pub(crate) async fn persist_native_assistant(
    orch: &ConversationOrchestrator,
    message: &ConversationMessage,
    response: &llm_runtime::HistoryResponse,
    request_id: Option<&str>,
    fence: crate::autonomous_tool_scheduler::ToolDispatchPublicationFence,
) -> Result<ConversationMessage, OrchestratorError> {
    let accepted = orch
        .mod_session_append_row(message, None, true, Some(Arc::new(fence.clone())))
        .await;
    if !fence.is_current() {
        return Err(internal("native binding publication was revoked"));
    }
    let ConversationMessage::Assistant { content, .. } = &accepted else {
        return Err(internal("native response became a non-assistant row"));
    };
    let expected_ack = receipt_acknowledgements(orch).await?;
    if !expected_ack.is_empty() {
        let ack = content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ProviderContent { value, .. }
                    if value["type"] == "lingxi_computer_receipt_ack" =>
                {
                    Some(value)
                }
                _ => None,
            })
            .ok_or_else(|| internal("append hook removed receipt acknowledgement"))?;
        if ack["call_ids"] != json!(expected_ack)
            || ack["provider_response_id"] != response.id
            || ack["receipts"]
                != serde_json::to_value(receipt_acknowledgement_bindings(orch).await?)
                    .map_err(internal)?
        {
            return Err(internal("append hook changed receipt acknowledgement"));
        }
    }
    if let Some(reference) =
        llm_runtime::computer::response_continuation(response).map_err(internal)?
    {
        let saved = serde_json::to_value(reference).map_err(internal)?;
        if !content.iter().any(|block| {
            matches!(block, ContentBlock::ProviderContent { value, .. }
            if value["type"] == "lingxi_computer_continuation" && value["continuation"] == saved)
        }) {
            return Err(internal(
                "append hook removed or changed the response continuation",
            ));
        }
    }
    let work = orch.computer_runtime.work.lock().await;
    let bindings = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ProviderContent { value, .. }
                if value["type"] == "lingxi_computer_binding" =>
            {
                Some(value)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = work
        .iter()
        .filter(|(_, member)| member.identity.provider_response_id == response.id)
        .map(|(id, _)| id.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut observed = std::collections::HashSet::new();
    for binding in bindings {
        let ids = binding["tool_use_ids"]
            .as_array()
            .ok_or_else(|| internal("native binding member IDs missing"))?;
        for id in ids {
            let id = ToolUseId::from(
                id.as_str()
                    .ok_or_else(|| internal("native member ID invalid"))?,
            );
            let member = work
                .get(&id)
                .ok_or_else(|| internal("native member does not match admitted response"))?;
            let original = llm_runtime::computer::canonical_response_content(
                response,
                protocol(member.call.context.provider),
            )
            .map_err(internal)?
            .into_iter()
            .filter(|block| computer_call_id(block) == Some(member.call.context.call_id.as_str()))
            .collect::<Vec<_>>();
            if !observed.insert(id.clone())
                || !expected.contains(&id)
                || binding["call"]
                    != serde_json::to_value(member.call.as_ref()).map_err(internal)?
                || binding["frame"] != serde_json::to_value(&member.frame).map_err(internal)?
                || binding["binding"] != serde_json::to_value(&member.binding).map_err(internal)?
                || binding["provider_response_id"] != response.id
                || binding["original_blocks"] != serde_json::to_value(original).map_err(internal)?
            {
                return Err(internal("append hook changed a native response binding"));
            }
            let calls = content.iter().filter(|block| matches!(block, ContentBlock::ToolUse { id: candidate, name, .. } if candidate == &id && name == member.tool.name())).count();
            if calls != 1 {
                return Err(internal(
                    "append hook removed, duplicated or renamed a native member",
                ));
            }
        }
    }
    if observed != expected {
        return Err(internal("append hook removed a native response member"));
    }
    drop(work);
    let writer = orch
        .transcript
        .jsonl_writer
        .as_ref()
        .ok_or_else(|| internal("native transcript unavailable"))?;
    let session_id = orch.session.lock().await.session_id;
    let usage = crate::conversation::assistant_usage_value(&response.usage);
    let mut row = orch.to_jsonl_message_with_inner_id(
        &accepted,
        &session_id.as_uuid().to_string(),
        orch.transcript.last_jsonl_uuid.lock().await.clone(),
        None,
        None,
        None,
        Some(&response.id),
        Some(&response.model),
        Some(&usage),
        request_id,
        None,
    );
    row.extra.insert(
        "modelProfile".into(),
        json!(orch.session.lock().await.model_profile),
    );
    let uuid = row.uuid.clone();
    let exact = session::jsonl::exact_json::message_utf16_overrides(&row);
    let saved_response = if expected_ack.is_empty() {
        None
    } else {
        Some(
            store_payload(
                orch,
                &format!("native-response:{}", response.id),
                &serde_json::to_value(StoredOutput::new(accepted.clone(), row.clone()))
                    .map_err(internal)?,
            )
            .await?,
        )
    };
    let payload = serde_json::to_value(row).map_err(internal)?;
    let mut persisted = None;
    let success = fence
        .commit_if_current(Box::pin(async {
            // Recovery can finish this exact successor if publication is interrupted.
            if let Some(saved) = saved_response {
                if let Err(error) = prepare_response_receipts(orch, &response.id, saved).await {
                    persisted = Some(Err(error));
                    return;
                }
            }
            persisted = Some(
                writer
                    .append_json_once_durable_for_session_with_tip_exact(
                        session_id,
                        &format!("native-response:{}", response.id),
                        payload,
                        exact,
                    )
                    .await
                    .map_err(internal),
            );
        }))
        .await;
    if !success {
        return Err(internal("native response durable publication revoked"));
    }
    persisted.ok_or_else(|| internal("native response persistence did not run"))??;
    *orch.transcript.last_jsonl_uuid.lock().await = Some(uuid.clone());
    for block in content {
        if let ContentBlock::ToolUse { id, .. } = block {
            orch.record_source_tool_assistant_uuid(id, uuid.clone())
                .await;
        }
    }
    orch.note_assistant_commit(&accepted).await;
    complete_receipts(orch, &response.id).await?;
    Ok(accepted)
}

pub(crate) fn has_native_binding(message: &ConversationMessage) -> bool {
    matches!(message, ConversationMessage::Assistant { content, .. } if content.iter().any(|block| matches!(block, ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_computer_binding")))
}

pub(crate) async fn publish_native_result(
    orch: &ConversationOrchestrator,
    message: &ConversationMessage,
) -> Result<bool, OrchestratorError> {
    let ConversationMessage::User { content, .. } = message else {
        return Ok(false);
    };
    let Some(id) = content.iter().find_map(|block| match block {
        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
        _ => None,
    }) else {
        return Ok(false);
    };
    let Some(work) = orch.computer_runtime.work.lock().await.get(id).cloned() else {
        return Ok(false);
    };
    let journal = orch
        .tool_execution_journal
        .as_ref()
        .ok_or_else(|| internal("native journal unavailable"))?;
    let mut record = journal
        .execution(&work.identity.execution_id())
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("native result has no execution fact"))?;
    if record.stage == ToolExecutionStage::Started {
        record.stage = ToolExecutionStage::OutcomeUnknown;
        journal
            .record_execution(record.clone())
            .await
            .map_err(internal)?;
    }
    if record.stage == ToolExecutionStage::OutcomeUnknown {
        record.outcome = Some(ToolExecutionOutcome::Unknown);
    }
    if record.stage != ToolExecutionStage::OutputPublished {
        if record.output.is_none() {
            record.output = Some(
                store_payload(
                    orch,
                    &record.execution_id(),
                    &serde_json::to_value(StoredOutput::new(
                        message.clone(),
                        prepare_stored_row(orch, message).await?,
                    ))
                    .map_err(internal)?,
                )
                .await?,
            );
            record.stage = ToolExecutionStage::OutputPrepared;
            journal
                .record_execution(record.clone())
                .await
                .map_err(internal)?;
        }
        let stored = load_output(
            record
                .output
                .as_ref()
                .ok_or_else(|| internal("missing saved output"))?,
        )
        .await?;
        persist_stored_row(
            orch,
            &stored.row,
            &format!("{}:output", record.execution_id()),
        )
        .await?;
        record.stage = ToolExecutionStage::OutputPublished;
        journal.record_execution(record).await.map_err(internal)?;
    }
    Ok(true)
}

pub(crate) async fn is_native_result(
    orch: &ConversationOrchestrator,
    message: &ConversationMessage,
) -> bool {
    let ConversationMessage::User { content, .. } = message else {
        return false;
    };
    let work = orch.computer_runtime.work.lock().await;
    content.iter().any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if work.contains_key(tool_use_id)))
}

/// Encode receipts solely from the published final ToolResult blocks.
pub(crate) async fn prepare_receipts(
    orch: &ConversationOrchestrator,
    ids: &[ToolUseId],
) -> Result<(), OrchestratorError> {
    let work = orch.computer_runtime.work.lock().await;
    let mut calls: Vec<(Arc<NativeComputerCall>, Vec<(ToolUseId, Work)>)> = Vec::new();
    for id in ids {
        let Some(member) = work.get(id).cloned() else {
            continue;
        };
        if let Some((_, members)) = calls.iter_mut().find(|(_, members)| {
            members[0].1.identity.call_identity() == member.identity.call_identity()
        }) {
            members.push((id.clone(), member));
        } else {
            calls.push((member.call.clone(), vec![(id.clone(), member)]));
        }
    }
    drop(work);
    for (call, mut members) in calls {
        members.sort_by_key(|(_, m)| m.index);
        let journal = orch
            .tool_execution_journal
            .as_ref()
            .ok_or_else(|| internal("native journal unavailable"))?;
        let receipt_id = format!(
            "native-receipt:{}",
            digest(&json!([
                members[0].1.identity.session_id,
                members[0].1.identity.provider_response_id,
                call.context.call_id
            ]))?
        );
        if let Some(record) = journal.receipt(&receipt_id).await.map_err(internal)? {
            if matches!(
                record.stage,
                NativeReceiptStage::Prepared | NativeReceiptStage::NotSubmitted
            ) {
                orch.computer_runtime
                    .state
                    .lock()
                    .await
                    .pending_receipts
                    .push(record);
                continue;
            }
            return Err(internal(
                "native receipt already submitted; input and request replay refused",
            ));
        }
        let mut results = Vec::new();
        let mut execution_ids = Vec::new();
        let mut acknowledged_safety_checks = Vec::new();
        for (id, member) in &members {
            let record = journal
                .execution(&member.identity.execution_id())
                .await
                .map_err(internal)?
                .ok_or_else(|| internal("native member missing durable result"))?;
            if record.stage != ToolExecutionStage::OutputPublished {
                return Err(internal("native output is not reliably published"));
            }
            if member.index == 0 && record.outcome == Some(ToolExecutionOutcome::Succeeded) {
                acknowledged_safety_checks = record.acknowledged_safety_checks.clone();
            }
            let stored = load_output(
                record
                    .output
                    .as_ref()
                    .ok_or_else(|| internal("missing native output media"))?,
            )
            .await?;
            let ConversationMessage::User {
                content: blocks, ..
            } = stored.message
            else {
                return Err(internal("saved output is not a user tool result"));
            };
            let result = blocks
                .iter()
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        content_blocks,
                        ..
                    } if tool_use_id == id => Some((content.clone(), content_blocks.clone())),
                    _ => None,
                })
                .ok_or_else(|| internal("native output association invalid"))?;
            let mut visible = result.1.unwrap_or_default();
            for block in blocks {
                if let ContentBlock::Image { .. } = block {
                    visible.push(serde_json::to_value(block).map_err(internal)?);
                }
            }
            let status = match record.outcome {
                Some(ToolExecutionOutcome::Succeeded) => NativeExecutionStatus::Succeeded,
                Some(ToolExecutionOutcome::Denied) => NativeExecutionStatus::Denied,
                Some(ToolExecutionOutcome::Cancelled) => NativeExecutionStatus::Cancelled,
                Some(ToolExecutionOutcome::Skipped) => NativeExecutionStatus::Skipped,
                Some(ToolExecutionOutcome::Failed) => NativeExecutionStatus::Failed,
                _ => NativeExecutionStatus::OutcomeUnknown,
            };
            results.push(NativeComputerResult {
                operation_index: member.index,
                status,
                content: result.0,
                blocks: Some(visible),
            });
            execution_ids.push(record.execution_id());
        }
        if results
            .iter()
            .any(|result| result.status == NativeExecutionStatus::OutcomeUnknown)
        {
            recovery::abandon_calls(
                orch, members[0].1.identity.session_id, call.context.provider,
                std::collections::BTreeSet::from([members[0].1.identity.call_identity()]),
                "Computer input outcome is unknown. Do not repeat these inputs; obtain a fresh observation through the same provider's computer function.",
            ).await?;
            continue;
        }
        let input = ComputerReceiptInput {
            results,
            acknowledged_safety_checks,
        };
        let block = match encode_computer_receipt(&call, &input) {
            Ok(block) => block,
            Err(error) => {
                let facts = json!({"call_id":call.context.call_id,"reason":error.to_string()});
                let record = NativeReceiptRecord {
                    response: None,
                    session_id: members[0].1.identity.session_id,
                    receipt_id: receipt_id.clone(),
                    execution_ids,
                    binding: members[0].1.binding.clone(),
                    stage: NativeReceiptStage::CannotResume,
                    submission_attempt: 0,
                    receipt: DurableToolOutput {
                        digest: digest(&facts)?,
                        payload: facts,
                        media_refs: Vec::new(),
                    },
                    provider_response_id: None,
                };
                journal
                    .record_receipt(record.clone())
                    .await
                    .map_err(internal)?;
                orch.computer_runtime
                    .state
                    .lock()
                    .await
                    .pending_receipts
                    .push(record);
                return Err(internal(error));
            }
        };
        let message = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ProviderContent {
                protocol: protocol_name(call.context.provider),
                value: json!({"type":"lingxi_computer_receipt","call_id":call.context.call_id,"provider_response_id":members[0].1.identity.provider_response_id,"receipt_id":receipt_id,"block":block}),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let row = prepare_stored_row(orch, &message).await?;
        let stored = StoredReceipt {
            block,
            utf16_overrides: session::jsonl::exact_json::message_utf16_overrides(&row),
            row,
            message: message.clone(),
            call: call.as_ref().clone(),
            frame: members[0].1.frame.clone(),
        };
        let payload = store_payload(
            orch,
            &receipt_id,
            &serde_json::to_value(&stored).map_err(internal)?,
        )
        .await?;
        let record = NativeReceiptRecord {
            response: None,
            session_id: members[0].1.identity.session_id,
            receipt_id: receipt_id.clone(),
            execution_ids,
            binding: members[0].1.binding.clone(),
            stage: NativeReceiptStage::Prepared,
            submission_attempt: 0,
            receipt: payload,
            provider_response_id: None,
        };
        let ack = journal
            .record_receipt(record.clone())
            .await
            .map_err(internal)?;
        if ack.duplicate {
            return Err(internal("native receipt preparation conflicted"));
        }
        persist_stored_row(orch, &stored.row, &format!("{receipt_id}:history")).await?;
        orch.session.lock().await.history.push(message);
        orch.computer_runtime
            .state
            .lock()
            .await
            .pending_receipts
            .push(record);
    }
    Ok(())
}

pub(crate) async fn has_pending_receipts(orch: &ConversationOrchestrator) -> bool {
    !orch
        .computer_runtime
        .state
        .lock()
        .await
        .pending_receipts
        .is_empty()
}

async fn receipt_acknowledgements(
    orch: &ConversationOrchestrator,
) -> Result<Vec<String>, OrchestratorError> {
    let pending = orch
        .computer_runtime
        .state
        .lock()
        .await
        .pending_receipts
        .clone();
    let mut acknowledged = Vec::new();
    if let Some(journal) = &orch.tool_execution_journal {
        for receipt in pending {
            let record = journal
                .receipt(&receipt.receipt_id)
                .await
                .map_err(internal)?
                .ok_or_else(|| internal("native receipt disappeared"))?;
            if record.stage != NativeReceiptStage::Submitted {
                return Err(internal(
                    "response did not follow a durably submitted receipt",
                ));
            }
            for execution in &record.execution_ids {
                if let Some(execution) = journal.execution(execution).await.map_err(internal)? {
                    if !acknowledged.contains(&execution.identity.provider_call_id) {
                        acknowledged.push(execution.identity.provider_call_id);
                    }
                }
            }
        }
    }
    Ok(acknowledged)
}

async fn receipt_acknowledgement_bindings(
    orch: &ConversationOrchestrator,
) -> Result<Vec<ReceiptAcknowledgement>, OrchestratorError> {
    let pending = orch
        .computer_runtime
        .state
        .lock()
        .await
        .pending_receipts
        .clone();
    let mut bindings = Vec::new();
    if let Some(journal) = &orch.tool_execution_journal {
        for receipt in pending {
            let record = journal
                .receipt(&receipt.receipt_id)
                .await
                .map_err(internal)?
                .ok_or_else(|| internal("native receipt disappeared"))?;
            if record.stage != NativeReceiptStage::Submitted {
                return Err(internal("receipt was not submitted"));
            }
            bindings.push(ReceiptAcknowledgement::from_record(&record));
        }
    }
    Ok(bindings)
}

async fn prepare_response_receipts(
    orch: &ConversationOrchestrator,
    response_id: &str,
    saved: DurableToolOutput,
) -> Result<(), OrchestratorError> {
    let pending = orch
        .computer_runtime
        .state
        .lock()
        .await
        .pending_receipts
        .clone();
    if let Some(journal) = &orch.tool_execution_journal {
        for receipt in pending {
            let mut record = journal
                .receipt(&receipt.receipt_id)
                .await
                .map_err(internal)?
                .ok_or_else(|| internal("native receipt disappeared"))?;
            if record.stage != NativeReceiptStage::Submitted {
                return Err(internal("response was not preceded by submission"));
            }
            record.stage = NativeReceiptStage::ResponsePrepared;
            record.provider_response_id = Some(response_id.into());
            record.response = Some(saved.clone());
            journal.record_receipt(record).await.map_err(internal)?;
        }
    }
    Ok(())
}

async fn complete_receipts(
    orch: &ConversationOrchestrator,
    response_id: &str,
) -> Result<Vec<String>, OrchestratorError> {
    let mut state = orch.computer_runtime.state.lock().await;
    let mut acknowledged = Vec::new();
    let mut completed = std::collections::HashSet::new();
    if let Some(journal) = &orch.tool_execution_journal {
        for receipt in &state.pending_receipts {
            let mut record = journal
                .receipt(&receipt.receipt_id)
                .await
                .map_err(internal)?
                .ok_or_else(|| internal("native receipt disappeared"))?;
            if record.stage != NativeReceiptStage::ResponsePrepared {
                return Err(internal(
                    "response did not follow a durably submitted receipt",
                ));
            }
            record.stage = NativeReceiptStage::ResponseReceived;
            record.provider_response_id = Some(response_id.into());
            journal
                .record_receipt(record.clone())
                .await
                .map_err(internal)?;
            completed.extend(record.execution_ids.iter().cloned());
            for execution in &record.execution_ids {
                if let Some(execution) = journal.execution(execution).await.map_err(internal)? {
                    if !acknowledged.contains(&execution.identity.provider_call_id) {
                        acknowledged.push(execution.identity.provider_call_id);
                    }
                }
            }
        }
    }
    state.pending_receipts.clear();
    drop(state);
    orch.computer_runtime
        .work
        .lock()
        .await
        .retain(|_, member| !completed.contains(&member.identity.execution_id()));
    Ok(acknowledged)
}

pub(crate) async fn cleanup(orch: &ConversationOrchestrator) -> Result<(), OrchestratorError> {
    let mut ctx = crate::turn_loop::streaming_tool_context_base(orch, Vec::new()).await;
    let mut state = orch.computer_runtime.state.lock().await;
    ctx.origin_session_id = state.session_id;
    state.next_native = false;
    state.active = None;
    drop(state);
    let mut tools = orch
        .computer_runtime
        .work
        .lock()
        .await
        .values()
        .map(|w| w.tool.clone())
        .collect::<Vec<_>>();
    if let Some(tool) = orch.tools.find_registered("computer") {
        tools.push(tool);
    }
    let mut unique: Vec<Arc<dyn Tool>> = Vec::new();
    for tool in tools {
        if tool.native_computer_capabilities().is_some()
            && !unique.iter().any(|old| Arc::ptr_eq(old, &tool))
        {
            unique.push(tool);
        }
    }
    for tool in unique {
        tool.cleanup_computer_inputs(&ctx).await.map_err(internal)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
