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
}

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

impl ComputerReceiptSubmission {
    pub async fn before_submit(&self) -> Result<(), LlmError> {
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
                .record_receipt(submitted)
                .await
                .map_err(|error| LlmError::InvalidRequest {
                    message: format!(
                        "computer receipt submission was not durably acknowledged: {error}"
                    ),
                })?;
            if ack.duplicate {
                return Err(LlmError::InvalidRequest { message:"computer receipt submission was already admitted; automatic resubmission is forbidden".into() });
            }
        }
        Ok(())
    }

    /// Only call after the final dispatch callback rejected, proving zero sends.
    pub(crate) async fn not_submitted(&self) -> Result<(), LlmError> {
        use lingxi_core::host::NativeReceiptStage;
        for prepared in &self.receipts {
            let mut record = self
                .journal
                .receipt(&prepared.receipt_id)
                .await
                .map_err(|e| LlmError::InvalidRequest {
                    message: e.to_string(),
                })?
                .ok_or_else(|| LlmError::InvalidRequest {
                    message: "submitted receipt disappeared".into(),
                })?;
            if record.stage != NativeReceiptStage::Submitted
                || Some(record.submission_attempt) != prepared.submission_attempt.checked_add(1)
            {
                return Err(LlmError::InvalidRequest {
                    message: "unsent receipt has a different submission attempt".into(),
                });
            }
            record.stage = NativeReceiptStage::NotSubmitted;
            self.journal
                .record_receipt(record)
                .await
                .map_err(|e| LlmError::InvalidRequest {
                    message: e.to_string(),
                })?;
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
        .try_with(|scope| matches!(scope, ComputerRequestScope::Auxiliary))
        .unwrap_or(false)
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
    let binding: lingxi_core::host::NativeContinuationBinding = serde_json::from_value(
        response
            .provider_metadata
            .get("llm_client")?
            .get("computer_binding")?
            .clone(),
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
    let message = crate::Message { api_output_config: None,
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
        .get("llm_client")
        .and_then(|metadata| metadata.get("continuation"))
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
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
        if value["type"] == "lingxi_computer_binding")
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
    async fn pure_preparation_never_submits_a_receipt() {
        let config: crate::ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = crate::ModelRuntime::from_config(config).unwrap();
        let journal = Arc::new(Journal::default());
        let mut request = crate::LlmRequest::new("model").with_user_text("test");
        request.execution.computer_submission = Some(submission(journal.clone()));
        let prepared = client.prepare(&request).await.unwrap();
        assert!(journal.0.lock().unwrap().is_empty());
        prepared.before_computer_submit().await.unwrap();
        assert_eq!(journal.0.lock().unwrap().len(), 1);
        let cloned = client.prepare(&request).await.unwrap();
        assert!(cloned.before_computer_submit().await.is_err());
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
        scope_computer_request(
            Some(ComputerRequestProjection {
                native: None,
                continuation: None,
                binding: None,
                submission: None,
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
                value: json!({"type":"lingxi_computer_binding"}),
            }],
            stop_reason: Some("tool_use".into()),
            stop_details: None,
            usage: Default::default(),
            cost: None,
            provider_metadata: serde_json::Value::Null,
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
