//! Request-local native computer projection and durable response recovery.
//!
//! Tool selection and execution belong to the host's ordinary tool lifecycle.
//! This boundary projects the already selected builtin and restores SDK data.
use crate::{ContinuationRef, HistoryResponse, LlmError};
use lingxi_llm_client::protocol as wire;
use std::{future::Future, sync::Arc};

pub use wire::computer::{
    ComputerCapabilities, ComputerFrame, NativeComputerCall, NativeComputerProvider,
};

#[derive(Debug, Clone)]
pub struct ComputerNativeDeclaration {
    pub provider: NativeComputerProvider,
    pub capabilities: ComputerCapabilities,
    pub frame: ComputerFrame,
}

/// A function round may still carry the preceding native round's continuation.
#[derive(Debug, Clone)]
pub struct ComputerRequestProjection {
    pub native: Option<ComputerNativeDeclaration>,
    pub continuation: Option<ContinuationRef>,
    pub binding: Option<lingxi_core::host::NativeContinuationBinding>,
    pub submission: Option<Arc<ComputerReceiptSubmission>>,
    /// Project completed native computer rows through their ordinary tool
    /// transcript when a profile boundary requires a fresh provider request.
    pub ordinary_history: bool,
    /// Profile epoch that owns native bindings and receipts in this request.
    pub context_epoch: u64,
}

#[derive(Clone)]
pub struct ComputerReceiptSubmission {
    pub journal: Arc<dyn lingxi_core::host::ToolExecutionJournal>,
    pub receipts: Vec<lingxi_core::host::NativeReceiptRecord>,
}

impl std::fmt::Debug for ComputerReceiptSubmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComputerReceiptSubmission")
            .field("receipts", &self.receipts)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ComputerReceiptSubmission {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.journal, &other.journal) && self.receipts == other.receipts
    }
}

/// Owns the exact Submitted acknowledgements for a dispatch that has not yet
/// passed final SDK admission. A duplicate acknowledgement never grants ownership.
pub(crate) struct PendingComputerSubmission {
    runtime: tokio::runtime::Handle,
    journal: Arc<dyn lingxi_core::host::ToolExecutionJournal>,
    submitted: Vec<lingxi_core::host::NativeReceiptRecord>,
}

impl PendingComputerSubmission {
    async fn rollback(
        journal: &dyn lingxi_core::host::ToolExecutionJournal,
        submitted: &[lingxi_core::host::NativeReceiptRecord],
    ) -> Result<(), LlmError> {
        use lingxi_core::host::NativeReceiptStage;
        let mut first_error = None;
        for owned in submitted {
            let result = async {
                if journal.receipt(&owned.receipt_id).await?.as_ref() == Some(owned) {
                    let mut record = owned.clone();
                    record.stage = NativeReceiptStage::NotSubmitted;
                    journal.record_receipt(record).await?;
                }
                Ok::<_, lingxi_core::host::ToolJournalError>(())
            }
            .await;
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), |error| {
            Err(LlmError::InvalidRequest {
                message: format!("unsent computer receipt cleanup failed: {error}"),
            })
        })
    }

    /// Called synchronously after the final admission callback succeeds.
    pub(crate) fn dispatched(&mut self) {
        self.submitted.clear();
    }

    /// Await durable cleanup on an ordinary pre-dispatch failure. Drop also
    /// covers cancellation of this wait. The journal fences stage and attempt.
    pub(crate) async fn close(&mut self) -> Result<(), LlmError> {
        Self::rollback(self.journal.as_ref(), &self.submitted).await?;
        self.submitted.clear();
        Ok(())
    }
}

impl Drop for PendingComputerSubmission {
    fn drop(&mut self) {
        if self.submitted.is_empty() {
            return;
        }
        let journal = self.journal.clone();
        let submitted = std::mem::take(&mut self.submitted);
        self.runtime.spawn(async move {
            if let Err(error) = Self::rollback(journal.as_ref(), &submitted).await {
                tracing::error!(%error, "failed to release unsent computer receipts");
            }
        });
    }
}

impl ComputerReceiptSubmission {
    pub async fn before_submit(&self) -> Result<(), LlmError> {
        Arc::new(self.clone())
            .prepare_dispatch()
            .await?
            .dispatched();
        Ok(())
    }

    pub(crate) async fn prepare_dispatch(
        self: &Arc<Self>,
    ) -> Result<PendingComputerSubmission, LlmError> {
        // A cancelled caller must not drop a journal write between its commit
        // and acknowledgement. The owned task finishes it; dropping its result
        // rolls back only the entries that this caller actually acquired.
        let submission = self.clone();
        tokio::spawn(async move {
            let mut guard = PendingComputerSubmission {
                runtime: tokio::runtime::Handle::current(),
                journal: submission.journal.clone(),
                submitted: Vec::new(),
            };
            if let Err(error) = submission.prepare_receipts(&mut guard).await {
                guard.close().await?;
                return Err(error);
            }
            Ok(guard)
        })
        .await
        .map_err(|error| LlmError::InvalidRequest {
            message: format!("computer receipt preparation interrupted: {error}"),
        })?
    }

    async fn prepare_receipts(
        &self,
        guard: &mut PendingComputerSubmission,
    ) -> Result<(), LlmError> {
        use lingxi_core::host::NativeReceiptStage;
        if self.receipts.is_empty() {
            return Err(LlmError::InvalidRequest {
                message: "computer receipt submission has no prepared receipts".into(),
            });
        }
        for receipt in &self.receipts {
            if !matches!(
                receipt.stage,
                NativeReceiptStage::Prepared | NativeReceiptStage::NotSubmitted
            ) {
                return Err(LlmError::InvalidRequest {
                    message: "only prepared computer receipts may be submitted".into(),
                });
            }
            let current = self
                .journal
                .receipt(&receipt.receipt_id)
                .await
                .map_err(|e| LlmError::InvalidRequest {
                    message: e.to_string(),
                })?;
            if current.as_ref().is_some_and(|current| current != receipt) {
                return Err(LlmError::InvalidRequest {
                    message: "computer receipt submission snapshot is stale".into(),
                });
            }
            let mut submitted = receipt.clone();
            submitted.stage = NativeReceiptStage::Submitted;
            submitted.submission_attempt =
                submitted.submission_attempt.checked_add(1).ok_or_else(|| {
                    LlmError::InvalidRequest {
                        message: "computer receipt submission attempt overflow".into(),
                    }
                })?;
            let ack = self
                .journal
                .record_receipt(submitted.clone())
                .await
                .map_err(|error| LlmError::InvalidRequest {
                    message: format!(
                        "computer receipt submission was not durably acknowledged: {error}"
                    ),
                })?;
            if ack.duplicate {
                return Err(LlmError::InvalidRequest { message:"computer receipt submission was already admitted; automatic resubmission is forbidden".into() });
            }
            guard.submitted.push(submitted);
        }
        Ok(())
    }
}

#[derive(Clone)]
enum ComputerRequestScope {
    Main(Option<ComputerRequestProjection>),
    Auxiliary,
}

tokio::task_local! {
    static COMPUTER_REQUEST: ComputerRequestScope;
}

pub async fn scope_computer_request<F: Future>(
    projection: Option<ComputerRequestProjection>,
    future: F,
) -> F::Output {
    COMPUTER_REQUEST
        .scope(ComputerRequestScope::Main(projection), Box::pin(future))
        .await
}

/// Build an independent query without inheriting the parent's desktop
/// declaration, continuation boundary or receipt submission authority.
pub fn without_computer_request<F: FnOnce() -> R, R>(build: F) -> R {
    COMPUTER_REQUEST.sync_scope(ComputerRequestScope::Auxiliary, build)
}

pub(crate) fn uses_auxiliary_history_projection() -> bool {
    COMPUTER_REQUEST
        .try_with(|scope| match scope {
            ComputerRequestScope::Auxiliary => true,
            ComputerRequestScope::Main(Some(_)) => false,
            ComputerRequestScope::Main(None) => false,
        })
        .unwrap_or(false)
}

pub(crate) fn uses_ordinary_computer_history() -> bool {
    COMPUTER_REQUEST
        .try_with(|scope| matches!(scope, ComputerRequestScope::Main(Some(projection)) if projection.ordinary_history))
        .unwrap_or(false)
}

pub(crate) fn current_computer_context_epoch() -> u64 {
    COMPUTER_REQUEST
        .try_with(|scope| match scope {
            ComputerRequestScope::Main(Some(projection)) => projection.context_epoch,
            _ => 0,
        })
        .unwrap_or(0)
}

pub(crate) fn apply_request_projection(request: &mut crate::LlmRequest) -> Result<(), LlmError> {
    let Ok(ComputerRequestScope::Main(Some(projection))) = COMPUTER_REQUEST.try_with(Clone::clone)
    else {
        return Ok(());
    };
    request.execution.computer_request = true;
    request.execution.computer_native = projection.native.is_some();
    request.execution.expected_computer_binding = projection.binding;
    request.execution.computer_submission = projection.submission;
    request.input.continuation = projection.continuation;
    if let Some(native) = projection.native {
        let protocol = match native.provider {
            NativeComputerProvider::OpenAi => wire::ProtocolFamily::OpenAiResponses,
            NativeComputerProvider::Anthropic => wire::ProtocolFamily::AnthropicMessages,
            NativeComputerProvider::Gemini => wire::ProtocolFamily::GeminiInteractions,
        };
        if request
            .execution
            .input_protocol
            .is_some_and(|selected| selected != protocol)
        {
            return Err(LlmError::InvalidRequest {
                message: "native computer declaration does not match the selected route protocol"
                    .into(),
            });
        }
        // The caller has selected the actual builtin from the filtered registry.
        // MCP tools retain their distinct wire names and declarations.
        if !request
            .input
            .tools
            .iter()
            .any(|tool| tool.name == "computer")
        {
            return Err(LlmError::InvalidRequest {
                message: "native computer projection requires the selected builtin declaration"
                    .into(),
            });
        }
        request.input.tools.retain(|tool| tool.name != "computer");
        wire::computer::declare_computer_tool(
            &mut request.input,
            native.provider,
            &native.capabilities,
            &native.frame,
        )
        .map_err(crate::upstream::error)?;
    }
    Ok(())
}

pub(crate) fn current_continuation() -> Option<ContinuationRef> {
    COMPUTER_REQUEST
        .try_with(|scope| match scope {
            ComputerRequestScope::Main(projection) => projection
                .as_ref()
                .and_then(|projection| projection.continuation.clone()),
            ComputerRequestScope::Auxiliary => None,
        })
        .ok()
        .flatten()
}

pub fn response_execution_binding(
    response: &HistoryResponse,
) -> Option<lingxi_core::host::NativeContinuationBinding> {
    let binding: lingxi_core::host::NativeContinuationBinding = serde_json::from_str(
        response
            .provider_metadata
            .payload_at("/llm_client/computer_binding")?
            .as_json_source(),
    )
    .ok()?;
    [
        &binding.account,
        &binding.profile,
        &binding.model,
        &binding.endpoint,
        &binding.protocol,
    ]
    .iter()
    .all(|value| !value.trim().is_empty())
    .then_some(binding)
}

/// Recover canonical blocks, including provider metadata retained as companions.
pub fn canonical_response_content(
    response: &HistoryResponse,
    protocol: wire::ProtocolFamily,
) -> Result<Vec<wire::ContentBlock>, LlmError> {
    let message = crate::Message {
        api_output_config: None,
        role: "assistant".into(),
        content: response.content.clone(),
    };
    crate::convert::input_projection::canonical_message_content(&message, protocol)
}

pub fn response_continuation(
    response: &HistoryResponse,
) -> Result<Option<ContinuationRef>, LlmError> {
    let Some(value) = response
        .provider_metadata
        .payload_at("/llm_client/continuation")
        .filter(|value| value.as_json_source().trim() != "null")
    else {
        return Ok(None);
    };
    serde_json::from_str(value.as_json_source())
        .map(Some)
        .map_err(|error| LlmError::InvalidRequest {
            message: format!("invalid durable computer continuation: {error}"),
        })
}

/// Fingerprint captured credentials without placing secrets in durable history.
/// Opaque tokens are conservatively invalidated when their material changes.
pub(crate) fn credential_account_scope(
    credential: Option<&crate::Credential>,
) -> Result<String, LlmError> {
    use sha2::{Digest, Sha256};
    let Some(credential) = credential else {
        return Err(LlmError::InvalidRequest {
            message: "native computer requests require a captured account credential".into(),
        });
    };
    let (kind, identity) = match credential {
        crate::Credential::ApiKey(key) => ("api_key", key.as_str()),
        crate::Credential::BearerToken(token) => ("bearer", token.as_str()),
        crate::Credential::AnthropicOAuth { access_token, .. } => {
            ("anthropic_oauth", access_token.as_str())
        }
        crate::Credential::ChatGptOAuth {
            account_id,
            access_token,
            ..
        } => match account_id.as_deref() {
            Some(account) if !account.is_empty() => ("chatgpt_account", account),
            _ => ("chatgpt_token", access_token.as_str()),
        },
        crate::Credential::AwsSigV4 { access_key_id, .. } => {
            ("aws_account", access_key_id.as_str())
        }
    };
    if identity.is_empty() {
        return Err(LlmError::InvalidRequest {
            message: "native computer account identity is empty".into(),
        });
    }
    let mut hash = Sha256::new();
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update(identity.as_bytes());
    Ok(format!("computer:{:x}", hash.finalize()))
}

/// Decode complete output without executing or reconstructing already bound calls.
pub fn decode_computer_calls(
    response: &HistoryResponse,
    provider: NativeComputerProvider,
    frame: &ComputerFrame,
) -> Result<Vec<NativeComputerCall>, LlmError> {
    // Durable binding rows describe previously admitted calls. They must never
    // become fresh work when a saved response is inspected during recovery.
    if response.content.iter().any(|block| {
        matches!(block,
        crate::ContentBlock::ProviderContent { value, .. }
        if value.string_at("/type").as_deref() == Some("lingxi_computer_binding"))
    }) {
        return Ok(Vec::new());
    }
    let protocol = match provider {
        NativeComputerProvider::OpenAi => wire::ProtocolFamily::OpenAiResponses,
        NativeComputerProvider::Anthropic => wire::ProtocolFamily::AnthropicMessages,
        NativeComputerProvider::Gemini => wire::ProtocolFamily::GeminiInteractions,
    };
    let content = canonical_response_content(response, protocol)?;
    let continuation = response_continuation(response)?;
    wire::computer::decode_computer_calls(provider, &content, continuation.as_ref(), frame)
        .map_err(crate::upstream::error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::ToolExecutionJournal;
    use serde_json::json;

    #[derive(Default)]
    pub(super) struct Journal(
        std::sync::Mutex<Vec<lingxi_core::host::NativeReceiptRecord>>,
        std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    );

    #[derive(Default)]
    pub(super) struct ReceiptTransport(std::sync::atomic::AtomicUsize);
    impl crate::test_support::FixtureTransport for ReceiptTransport {
        fn execute<'a>(
            &'a self,
            _: &'a crate::ProviderRequest,
        ) -> crate::BoxFuture<'a, Result<crate::ProviderResponse, LlmError>> {
            unreachable!()
        }
        fn open_stream<'a>(
            &'a self,
            _: &'a crate::ProviderRequest,
        ) -> crate::BoxFuture<'a, Result<crate::StreamingResponse, LlmError>> {
            unreachable!()
        }
        fn send_raw(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> crate::BoxFuture<'_, Result<lingxi_llm_client::StreamResponse, wire::LlmError>>
        {
            use futures::StreamExt;
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                let body = serde_json::to_vec(&json!({"id":"response","model":"model","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}})).unwrap();
                Ok(lingxi_llm_client::StreamResponse {
                    status: 200,
                    headers: vec![],
                    body: futures::stream::once(async { Ok(body.into()) }).boxed(),
                })
            })
        }
    }
    crate::impl_fixture_transport!(ReceiptTransport);
    #[async_trait::async_trait]
    impl lingxi_core::host::ToolExecutionJournal for Journal {
        async fn record_execution(
            &self,
            _: lingxi_core::host::ToolExecutionRecord,
        ) -> Result<lingxi_core::host::ToolJournalAck, lingxi_core::host::ToolJournalError>
        {
            unreachable!()
        }
        async fn execution(
            &self,
            _: &str,
        ) -> Result<
            Option<lingxi_core::host::ToolExecutionRecord>,
            lingxi_core::host::ToolJournalError,
        > {
            Ok(None)
        }
        async fn record_receipt(
            &self,
            receipt: lingxi_core::host::NativeReceiptRecord,
        ) -> Result<lingxi_core::host::ToolJournalAck, lingxi_core::host::ToolJournalError>
        {
            if receipt.stage == lingxi_core::host::NativeReceiptStage::Submitted {
                let gate = self.1.lock().unwrap().take();
                if let Some((entered, resume)) = gate {
                    entered.notify_one();
                    resume.notified().await;
                }
            }
            let mut records = self.0.lock().unwrap();
            let duplicate = records.iter().any(|existing| {
                existing.receipt_id == receipt.receipt_id
                    && existing.stage == receipt.stage
                    && existing.submission_attempt == receipt.submission_attempt
            });
            let event_id = receipt.event_id();
            if !duplicate {
                records.push(receipt);
            }
            Ok(lingxi_core::host::ToolJournalAck {
                event_id,
                journal_revision: records.len() as u64,
                duplicate,
            })
        }
        async fn receipt(
            &self,
            id: &str,
        ) -> Result<
            Option<lingxi_core::host::NativeReceiptRecord>,
            lingxi_core::host::ToolJournalError,
        > {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|receipt| receipt.receipt_id == id)
                .cloned())
        }
    }

    pub(super) fn submission(journal: Arc<Journal>) -> Arc<ComputerReceiptSubmission> {
        Arc::new(ComputerReceiptSubmission {
            journal,
            receipts: vec![lingxi_core::host::NativeReceiptRecord {
                response: None,
                session_id: lingxi_core::types::SessionId::new(),
                receipt_id: "receipt-1".into(),
                execution_ids: vec!["execution-1".into()],
                binding: lingxi_core::host::NativeContinuationBinding {
                    account: "account".into(),
                    profile: "profile".into(),
                    model: "model".into(),
                    endpoint: "endpoint".into(),
                    protocol: "anthropic_messages".into(),
                },
                stage: lingxi_core::host::NativeReceiptStage::Prepared,
                submission_attempt: 0,
                receipt: lingxi_core::host::DurableToolOutput {
                    digest: "digest".into(),
                    payload: json!({}),
                    media_refs: vec![],
                },
                provider_response_id: None,
            }],
        })
    }

    #[tokio::test]
    async fn receipt_submission_requires_a_fresh_durable_ack() {
        let journal = Arc::new(Journal::default());
        let submission = submission(journal.clone());
        submission.before_submit().await.unwrap();
        assert_eq!(
            journal.0.lock().unwrap()[0].stage,
            lingxi_core::host::NativeReceiptStage::Submitted
        );
        assert!(submission.clone().before_submit().await.is_err());
        assert_eq!(journal.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_second_receipt_releases_only_this_submissions_acknowledgements() {
        use lingxi_core::host::NativeReceiptStage;
        let journal = Arc::new(Journal::default());
        let mut submission = submission(journal.clone());
        let mut second = submission.receipts[0].clone();
        second.receipt_id = "receipt-2".into();
        Arc::get_mut(&mut submission)
            .unwrap()
            .receipts
            .push(second.clone());
        second.stage = NativeReceiptStage::Submitted;
        second.submission_attempt = 1;
        journal.record_receipt(second.clone()).await.unwrap();

        assert!(submission.prepare_dispatch().await.is_err());
        let first = journal.receipt("receipt-1").await.unwrap().unwrap();
        assert_eq!(
            (first.stage, first.submission_attempt),
            (NativeReceiptStage::NotSubmitted, 1)
        );
        assert_eq!(journal.receipt("receipt-2").await.unwrap(), Some(second));
    }

    #[tokio::test]
    async fn cancelling_a_waiting_dispatch_releases_its_late_journal_ack() {
        let journal = Arc::new(Journal::default());
        let submission = submission(journal.clone());
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        *journal.1.lock().unwrap() = Some((entered.clone(), resume.clone()));
        let waiter = tokio::spawn(async move { submission.prepare_dispatch().await });
        entered.notified().await;
        waiter.abort();
        assert!(waiter.await.is_err());
        resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if journal
                    .receipt("receipt-1")
                    .await
                    .unwrap()
                    .is_some_and(|receipt| {
                        receipt.stage == lingxi_core::host::NativeReceiptStage::NotSubmitted
                            && receipt.submission_attempt == 1
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late acknowledgement released");
    }

    #[tokio::test]
    async fn pure_preparation_never_submits_a_receipt() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = crate::ModelRuntime::from_config(config).unwrap();
        let journal = Arc::new(Journal::default());
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.input.max_tokens = Some(4096);
        request.execution.computer_submission = Some(submission(journal.clone()));
        let prepared = client.prepare(&request).await.unwrap();
        assert!(journal.0.lock().unwrap().is_empty());
        let mut guard = prepared
            .prepare_computer_submission()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(journal.0.lock().unwrap().len(), 1);
        let cloned = client.prepare(&request).await.unwrap();
        assert!(cloned.prepare_computer_submission().await.is_err());
        guard.close().await.unwrap();
        assert_eq!(
            journal.receipt("receipt-1").await.unwrap().unwrap().stage,
            lingxi_core::host::NativeReceiptStage::NotSubmitted
        );
    }

    #[tokio::test]
    async fn gemini_management_and_native_prepare_through_host_api_key_authentication() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"gemini", "profile_name":"test", "base_url":"https://generativelanguage.googleapis.com/v1beta", "protocol":"gemini_interactions", "auth":"api_key", "credential":{"type":"static","id":"key"},
            "models":[{"display_model":"model","request_model":"gemini-3.8-flash","billing_model":"model","capabilities":{"streaming":true,"tools":true,"vision":true,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let credentials = Arc::new(crate::StaticCredentialProvider::new(
            crate::Credential::ApiKey("test-key".into()),
        ));
        let client = crate::ModelRuntime::from_config(config)
            .unwrap()
            .with_credential_provider(credentials);
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.execution.account_scope = Some("account".into());
        let prepared = client.prepare(&request).await.unwrap();
        assert!(prepared.provider_request.url.ends_with("/interactions"));
        let mut native =
            crate::upstream::request(&request, wire::ProtocolFamily::GeminiInteractions).unwrap();
        let capabilities = wire::computer::ComputerCapabilities {
            operations: vec![
                wire::computer::ComputerOperationKind::Click,
                wire::computer::ComputerOperationKind::Move,
                wire::computer::ComputerOperationKind::Drag,
                wire::computer::ComputerOperationKind::Scroll,
                wire::computer::ComputerOperationKind::Key,
                wire::computer::ComputerOperationKind::KeyDown,
                wire::computer::ComputerOperationKind::KeyUp,
                wire::computer::ComputerOperationKind::Type,
                wire::computer::ComputerOperationKind::Wait,
                wire::computer::ComputerOperationKind::Screenshot,
                wire::computer::ComputerOperationKind::MouseDown,
                wire::computer::ComputerOperationKind::MouseUp,
            ],
        };
        lingxi_llm_client::providers::google::computer::declare(
            &mut native,
            &capabilities,
            &wire::computer::ComputerFrame {
                width: 100,
                height: 100,
                geometry_version: "screen".into(),
            },
        )
        .unwrap();
        request.input.native_options = native.native_options;
        assert!(client
            .prepare(&request)
            .await
            .unwrap()
            .provider_request
            .url
            .ends_with("/interactions"));
    }

    #[tokio::test]
    async fn function_only_scope_preserves_profiles_without_credentials() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = crate::ModelRuntime::from_config(config).unwrap();
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.input.max_tokens = Some(4096);
        scope_computer_request(
            Some(ComputerRequestProjection {
                native: None,
                continuation: None,
                binding: None,
                submission: None,
                ordinary_history: false,
                context_epoch: 0,
            }),
            async {
                apply_request_projection(&mut request).unwrap();
            },
        )
        .await;
        assert!(client
            .prepare(&request)
            .await
            .unwrap()
            .computer_binding
            .is_none());
        request.execution.computer_native = true;
        assert!(client.prepare(&request).await.is_err());
    }

    #[tokio::test]
    async fn execute_persists_submission_before_transport_and_blocks_duplicates() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = crate::ModelRuntime::from_config(config).unwrap();
        let journal = Arc::new(Journal::default());
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.input.max_tokens = Some(4096);
        request.execution.computer_submission = Some(submission(journal.clone()));
        let transport = Arc::new(ReceiptTransport::default());
        client.execute(&request, transport.clone()).await.unwrap();
        assert_eq!(
            journal.0.lock().unwrap()[0].stage,
            lingxi_core::host::NativeReceiptStage::Submitted
        );
        assert!(client.execute(&request, transport.clone()).await.is_err());
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn direct_execute_exhausted_budget_preserves_prepared_receipt() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = crate::ModelRuntime::from_config(config).unwrap();
        let journal = Arc::new(Journal::default());
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.input.max_tokens = Some(4096);
        request.execution.computer_submission = Some(submission(journal.clone()));
        journal
            .record_receipt(
                request
                    .execution
                    .computer_submission
                    .as_ref()
                    .unwrap()
                    .receipts[0]
                    .clone(),
            )
            .await
            .unwrap();
        request.execution.api_attempts_left = Some(crate::ApiAttemptsLeft::new(0));
        let transport = Arc::new(ReceiptTransport::default());

        let error = client
            .execute(&request, transport.clone())
            .await
            .expect_err("an exhausted budget rejects before transport");

        assert!(matches!(error, LlmError::ApiAttemptsExhausted));
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let receipt = journal.receipt("receipt-1").await.unwrap().unwrap();
        assert_eq!(
            receipt.stage,
            lingxi_core::host::NativeReceiptStage::Prepared
        );
        assert_eq!(receipt.submission_attempt, 0);
    }

    #[tokio::test]
    async fn direct_stream_exhausted_budget_preserves_prepared_receipt() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = crate::ModelRuntime::from_config(config).unwrap();
        let journal = Arc::new(Journal::default());
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.input.max_tokens = Some(4096);
        request.stream = true;
        request.execution.computer_submission = Some(submission(journal.clone()));
        journal
            .record_receipt(
                request
                    .execution
                    .computer_submission
                    .as_ref()
                    .unwrap()
                    .receipts[0]
                    .clone(),
            )
            .await
            .unwrap();
        request.execution.api_attempts_left = Some(crate::ApiAttemptsLeft::new(0));
        let transport = Arc::new(ReceiptTransport::default());

        let error = match client.execute_stream(&request, transport.clone()).await {
            Err(error) => error,
            Ok(_) => panic!("an exhausted budget must reject before transport"),
        };

        assert!(matches!(error, LlmError::ApiAttemptsExhausted));
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let receipt = journal.receipt("receipt-1").await.unwrap().unwrap();
        assert_eq!(
            receipt.stage,
            lingxi_core::host::NativeReceiptStage::Prepared
        );
        assert_eq!(receipt.submission_attempt, 0);
    }

    #[tokio::test]
    async fn rejected_dispatch_admission_preserves_unsubmitted_receipts_for_both_drivers() {
        for stream in [false, true] {
            let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
                "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
                "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
            }]})).unwrap();
            let client = Arc::new(crate::ModelRuntime::from_config(config).unwrap());
            let journal = Arc::new(Journal::default());
            let receipt = submission(journal.clone());
            let transport = Arc::new(ReceiptTransport::default());
            let service = crate::ApiService::new(
                client.clone(),
                transport.clone(),
                Default::default(),
                Default::default(),
                "test",
                None,
                None,
            );
            let mut request = crate::LlmRequest::new("model").with_user_text("test");
            request.input.max_tokens = Some(4096);
            request.execution.computer_submission = Some(receipt.clone());
            request.execution.request_dispatch_admission =
                Some(crate::RequestDispatchAdmission::new(|| false));
            if stream {
                assert!(service.stream_request(request.clone()).await.is_err());
            } else {
                assert!(service
                    .execute_non_stream_request(
                        request.clone(),
                        crate::NonStreamingRequestClass::Main,
                        Default::default()
                    )
                    .await
                    .is_err());
            }
            assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(
                journal.0.lock().unwrap().is_empty(),
                "known unsent receipt must not acquire Submitted"
            );
            request.execution.request_dispatch_admission = None;
            let quota = Arc::new(crate::ScopeOutputQuota::new(
                std::num::NonZeroU32::new(4096).unwrap(),
                std::num::NonZeroU32::new(1).unwrap(),
            ));
            quota
                .set_dispatch_authorization(Arc::new(|| {
                    Box::pin(async {
                        tokio::task::yield_now().await;
                        Err(LlmError::PermissionDenied {
                            message: "scope denied".into(),
                        })
                    })
                }))
                .unwrap();
            let service = service.with_scope_output_quota(Some(quota));
            let result = if stream {
                service.stream_request(request.clone()).await.map(|_| ())
            } else {
                service
                    .execute_non_stream_request(
                        request.clone(),
                        crate::NonStreamingRequestClass::Main,
                        Default::default(),
                    )
                    .await
                    .map(|_| ())
            };
            assert!(
                matches!(result, Err(LlmError::PermissionDenied { .. })),
                "{result:?}"
            );
            assert!(journal.0.lock().unwrap().is_empty());
            assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            client.execute(&request, transport.clone()).await.unwrap();
            assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(
                journal.0.lock().unwrap()[0].stage,
                lingxi_core::host::NativeReceiptStage::Submitted
            );
        }
    }

    #[tokio::test]
    async fn admission_revoked_during_journal_write_never_sends_and_can_retry_safely() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for stream in [false, true] {
            let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
                "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
                "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
            }]})).unwrap();
            let client = Arc::new(crate::ModelRuntime::from_config(config).unwrap());
            let journal = Arc::new(Journal::default());
            let mut receipt = submission(journal.clone());
            let transport = Arc::new(ReceiptTransport::default());
            let service = Arc::new(crate::ApiService::new(
                client.clone(),
                transport.clone(),
                Default::default(),
                Default::default(),
                "test",
                None,
                None,
            ));
            let mut request = crate::LlmRequest::new("model").with_user_text("test");
            request.input.max_tokens = Some(4096);
            for attempt in 1..=2 {
                let admitted = Arc::new(AtomicBool::new(true));
                let flag = admitted.clone();
                request.execution.request_dispatch_admission =
                    Some(crate::RequestDispatchAdmission::new(move || {
                        flag.load(Ordering::SeqCst)
                    }));
                request.execution.computer_submission = Some(receipt.clone());
                let entered = Arc::new(tokio::sync::Notify::new());
                let resume = Arc::new(tokio::sync::Notify::new());
                *journal.1.lock().unwrap() = Some((entered.clone(), resume.clone()));
                let service = service.clone();
                let req = request.clone();
                let run = tokio::spawn(async move {
                    if stream {
                        service.stream_request(req).await.map(|_| ())
                    } else {
                        service
                            .execute_non_stream_request(
                                req,
                                crate::NonStreamingRequestClass::Main,
                                Default::default(),
                            )
                            .await
                            .map(|_| ())
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
                    .await
                    .unwrap();
                admitted.store(false, Ordering::SeqCst);
                resume.notify_one();
                assert!(matches!(
                    run.await.unwrap(),
                    Err(LlmError::RequestDispatchRejected {
                        prior_dispatch: false
                    })
                ));
                assert_eq!(transport.0.load(Ordering::SeqCst), 0);
                let latest = journal.receipt("receipt-1").await.unwrap().unwrap();
                assert_eq!(
                    latest.stage,
                    lingxi_core::host::NativeReceiptStage::NotSubmitted
                );
                assert_eq!(latest.submission_attempt, attempt);
                receipt = Arc::new(ComputerReceiptSubmission {
                    journal: journal.clone(),
                    receipts: vec![latest],
                });
            }
            request.execution.computer_submission = Some(receipt);
            request.execution.request_dispatch_admission = None;
            client.execute(&request, transport.clone()).await.unwrap();
            assert_eq!(transport.0.load(Ordering::SeqCst), 1);
            assert_eq!(
                journal
                    .receipt("receipt-1")
                    .await
                    .unwrap()
                    .unwrap()
                    .submission_attempt,
                3
            );
        }
    }

    #[tokio::test]
    async fn captured_execution_binding_rejects_an_account_or_endpoint_change() {
        let client = |endpoint: &str, key: &str| {
            let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
                "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":endpoint, "protocol":"anthropic_messages", "auth":"api_key", "credential":{"type":"static","id":"key"},
                "models":[{"display_model":"model","request_model":"wire-model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
            }]})).unwrap();
            crate::ModelRuntime::from_config(config)
                .unwrap()
                .with_credential_provider(Arc::new(crate::StaticCredentialProvider::new(
                    crate::Credential::ApiKey(key.into()),
                )))
        };
        let first = client("https://api.anthropic.com", "account-key");
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.input.max_tokens = Some(4096);
        request.execution.computer_request = true;
        let prepared = first.prepare(&request).await.unwrap();
        let binding = prepared.computer_binding.unwrap();
        assert_eq!(binding.profile, "test");
        assert_eq!(binding.model, "wire-model");
        assert_eq!(binding.protocol, "anthropic_messages");
        assert!(!binding.account.contains("account-key"));
        request.execution.expected_computer_binding = Some(binding);
        first.prepare(&request).await.unwrap();
        assert!(client("https://api.anthropic.com", "different-account-key")
            .prepare(&request)
            .await
            .is_err());
        assert!(client("https://different.example", "account-key")
            .prepare(&request)
            .await
            .is_err());
    }

    fn request() -> crate::LlmRequest {
        let mut request = crate::LlmRequest::new("model");
        request.input.tools = ["computer", "Read", "mcp__computer-use__click"]
            .into_iter()
            .map(|name| {
                serde_json::from_value(json!({
                    "name": name, "description": "tool", "input_schema": {"type":"object"}
                }))
                .unwrap()
            })
            .collect();
        request
    }

    fn native() -> ComputerRequestProjection {
        ComputerRequestProjection {
            native: Some(ComputerNativeDeclaration {
                provider: NativeComputerProvider::Anthropic,
                capabilities: ComputerCapabilities {
                    operations: vec![wire::computer::ComputerOperationKind::Screenshot],
                },
                frame: ComputerFrame {
                    width: 800,
                    height: 600,
                    geometry_version: "frame-1".into(),
                },
            }),
            continuation: None,
            binding: None,
            submission: None,
            ordinary_history: false,
            context_epoch: 0,
        }
    }

    #[tokio::test]
    async fn selected_builtin_projection_preserves_other_tools_and_scope() {
        let mut projected = request();
        scope_computer_request(Some(native()), async {
            apply_request_projection(&mut projected).unwrap();
        })
        .await;
        assert_eq!(
            projected
                .input
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Read", "mcp__computer-use__click"]
        );
        assert!(!projected.input.anthropic_client_toolsets().is_empty());
        assert!(projected.execution.computer_request);
        let mut ordinary = request();
        apply_request_projection(&mut ordinary).unwrap();
        assert_eq!(ordinary.input.tools.len(), 3);
        assert!(!ordinary.execution.computer_request);
    }

    #[tokio::test]
    async fn projection_is_local_to_concurrent_request_tasks() {
        let native = scope_computer_request(Some(native()), async {
            tokio::task::yield_now().await;
            let mut request = request();
            apply_request_projection(&mut request).unwrap();
            request
        });
        let ordinary = scope_computer_request(None, async {
            tokio::task::yield_now().await;
            let mut request = request();
            apply_request_projection(&mut request).unwrap();
            request
        });
        let (native, ordinary) = tokio::join!(native, ordinary);
        assert_eq!(native.input.tools.len(), 2);
        assert_eq!(ordinary.input.tools.len(), 3);
    }

    #[tokio::test]
    async fn function_round_retains_old_continuation() {
        let continuation: ContinuationRef = serde_json::from_value(json!({
            "protocol":"open_ai_responses", "response_id":"response-1", "provider_id":"openai",
            "profile_name":"openai", "endpoint_fingerprint":"endpoint", "account_scope":"account",
            "request_model":"model"
        }))
        .unwrap();
        let mut request = request();
        scope_computer_request(
            Some(ComputerRequestProjection {
                native: None,
                continuation: Some(continuation.clone()),
                binding: None,
                submission: None,
                ordinary_history: false,
                context_epoch: 0,
            }),
            async {
                apply_request_projection(&mut request).unwrap();
            },
        )
        .await;
        assert_eq!(request.input.continuation, Some(continuation));
        assert_eq!(request.input.tools.len(), 3);
    }

    #[test]
    fn captured_account_scope_changes_with_credentials_and_never_exposes_key() {
        let first = credential_account_scope(Some(&crate::Credential::ApiKey("secret-one".into())))
            .unwrap();
        let again = credential_account_scope(Some(&crate::Credential::ApiKey("secret-one".into())))
            .unwrap();
        let other = credential_account_scope(Some(&crate::Credential::ApiKey("secret-two".into())))
            .unwrap();
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert!(!first.contains("secret"));
    }

    #[test]
    fn saved_binding_is_never_decoded_as_new_work() {
        let response = HistoryResponse {
            id: "response".into(),
            model: "model".into(),
            content: vec![crate::ContentBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: wire::JsonPayload::from_generated_value(
                    &json!({"type":"lingxi_computer_binding"}),
                )
                .unwrap(),
            }],
            stop_reason: Some("tool_use".into()),
            stop_details: None,
            usage: Default::default(),
            cost: None,
            provider_metadata: wire::JsonPayload::parse("null").unwrap(),
            resumable: None,
        };
        let frame = ComputerFrame {
            width: 800,
            height: 600,
            geometry_version: "frame-1".into(),
        };
        assert!(
            decode_computer_calls(&response, NativeComputerProvider::OpenAi, &frame)
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
#[path = "computer/auxiliary_tests.rs"]
mod auxiliary_tests;
