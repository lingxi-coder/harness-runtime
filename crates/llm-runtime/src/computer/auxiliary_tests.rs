use super::tests::{submission, Journal, ReceiptTransport};
use super::*;
use crate::{
    ApiService, ClientConfig, Credential, LlmRequest, MessagesCreateRequest, ModelRuntime,
    StaticCredentialProvider,
};
use lingxi_core::host::{NativeReceiptStage, ToolExecutionJournal};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use serde_json::json;

fn user(text: &str) -> ConversationMessage {
    ConversationMessage::User { api_message_override: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: text.into(),
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    }
}

fn computer_tool() -> serde_json::Value {
    json!({"name":"computer","description":"Manage desktop","input_schema":{"type":"object"}})
}

fn service(protocol: wire::ProtocolFamily) -> ApiService {
    service_with_transport(protocol, Arc::new(ReceiptTransport::default()))
}

fn service_with_transport(
    protocol: wire::ProtocolFamily,
    transport: Arc<dyn crate::Transport>,
) -> ApiService {
    let (provider, endpoint) = match protocol {
        wire::ProtocolFamily::AnthropicMessages => {
            ("anthropic_first_party", "https://api.anthropic.com")
        }
        wire::ProtocolFamily::OpenAiResponses => ("open_ai", "https://api.openai.com/v1"),
        wire::ProtocolFamily::GeminiInteractions => {
            ("gemini", "https://generativelanguage.googleapis.com/v1beta")
        }
        _ => unreachable!(),
    };
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":provider,"profile_name":"test","base_url":endpoint,"protocol":protocol,
        "auth":"api_key","credential":{"type":"static","id":"fixture"},
        "models":[{"display_model":"model","request_model":if protocol == wire::ProtocolFamily::AnthropicMessages { "claude-opus-4-8" } else { "model" },"billing_model":"model",
        "capabilities":{"streaming":true,"tools":true,"vision":true,"documents":false,"reasoning":false,"structured_output":true}}]
    }]})).unwrap();
    ApiService::new(
        Arc::new(
            ModelRuntime::from_config(config)
                .unwrap()
                .with_credential_provider(Arc::new(StaticCredentialProvider::new(
                    Credential::ApiKey("fixture-key".into()),
                ))),
        ),
        transport,
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    )
}

fn auxiliary(
    service: &ApiService,
    messages: Vec<ConversationMessage>,
    tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> LlmRequest {
    service
        .build_side_query_request_with_thinking(
            "model",
            Some("test"),
            None,
            false,
            messages,
            tools,
            Some(512),
            None,
            vec![],
            None,
            None,
            None,
            Some("compaction"),
        )
        .unwrap()
}

fn assert_independent(request: &LlmRequest) {
    assert!(!request.execution.computer_request);
    assert!(!request.execution.computer_native);
    assert!(request.execution.computer_submission.is_none());
    assert!(request.execution.expected_computer_binding.is_none());
    assert!(request.input.continuation.is_none());
}

#[tokio::test]
async fn auxiliary_builders_and_dispatch_never_submit_parent_receipts() {
    let service = service(wire::ProtocolFamily::AnthropicMessages);
    let journal = Arc::new(Journal::default());
    let submission = submission(journal.clone());
    journal
        .record_receipt(submission.receipts[0].clone())
        .await
        .unwrap();
    let projection = ComputerRequestProjection {
        native: Some(ComputerNativeDeclaration {
            provider: NativeComputerProvider::Anthropic,
            capabilities: ComputerCapabilities {
                operations: vec![wire::computer::ComputerOperationKind::Screenshot],
            },
            frame: ComputerFrame {
                width: 800,
                height: 600,
                geometry_version: "frame".into(),
            },
        }),
        continuation: None,
        binding: None,
        submission: Some(submission.clone()),
    };
    scope_computer_request(Some(projection), async {
        for tools in [vec![], vec![computer_tool()]] {
            let request = auxiliary(&service, vec![user("summarize")], tools.clone());
            assert_independent(&request);
            assert_eq!(request.input.tools.len(), tools.len());
            assert!(request.input.anthropic_client_toolsets().is_empty());
            service.execute_side_query_request(request).await.unwrap();
            assert_eq!(
                journal.receipt("receipt-1").await.unwrap().unwrap().stage,
                NativeReceiptStage::Prepared
            );
        }
        let strict = service
            .build_json_schema_request_with_thinking(
                "model",
                Some("test"),
                None,
                vec![user("classify")],
                json!({"type":"object"}),
                Some(512),
                None,
                None,
                None,
                Some("hook_prompt"),
            )
            .unwrap();
        assert_independent(&strict);
        let main = MessagesCreateRequest::new(
            "model",
            Some("test"),
            None,
            vec![user("continue")],
            vec![computer_tool()],
        );
        service.messages_create(main).await.unwrap();
        let submitted = journal.receipt("receipt-1").await.unwrap().unwrap();
        assert_eq!(submitted.stage, NativeReceiptStage::Submitted);
        assert_eq!(submitted.submission_attempt, 1);
        // A later auxiliary call must also work while the main receipt is in flight.
        service
            .execute_side_query_request(auxiliary(&service, vec![user("summarize again")], vec![]))
            .await
            .unwrap();
        assert_eq!(
            journal.receipt("receipt-1").await.unwrap().unwrap(),
            submitted
        );
    })
    .await;
}

#[tokio::test]
async fn scheduled_main_request_keeps_its_native_projection_and_receipt_authority() {
    let service = service(wire::ProtocolFamily::AnthropicMessages);
    let journal = Arc::new(Journal::default());
    let submission = submission(journal.clone());
    journal
        .record_receipt(submission.receipts[0].clone())
        .await
        .unwrap();
    scope_computer_request(
        Some(ComputerRequestProjection {
            native: Some(ComputerNativeDeclaration {
                provider: NativeComputerProvider::Anthropic,
                capabilities: ComputerCapabilities {
                    operations: vec![wire::computer::ComputerOperationKind::Screenshot],
                },
                frame: ComputerFrame {
                    width: 800,
                    height: 600,
                    geometry_version: "scheduled-frame".into(),
                },
            }),
            continuation: None,
            binding: None,
            submission: Some(submission.clone()),
        }),
        async {
            let mut request = MessagesCreateRequest::new(
                "model",
                Some("test"),
                None,
                vec![user("scheduled desktop task")],
                vec![computer_tool()],
            );
            request.opts.query_source = Some("scheduled_task".into());
            let request = service
                .build_scheduled_request(
                    request,
                    crate::model::thinking::ThinkingConfig::Disabled,
                    None,
                )
                .unwrap();
            assert!(request.execution.computer_native);
            assert!(Arc::ptr_eq(
                request.execution.computer_submission.as_ref().unwrap(),
                &submission
            ));
            assert!(request.input.tools.is_empty());
            assert!(!request.input.anthropic_client_toolsets().is_empty());
            service
                .execute_non_stream_request(
                    request,
                    crate::NonStreamingRequestClass::Auxiliary,
                    Default::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                journal.receipt("receipt-1").await.unwrap().unwrap().stage,
                NativeReceiptStage::Submitted
            );
        },
    )
    .await;
}

#[tokio::test]
async fn auxiliary_history_conversion_does_not_use_the_parent_continuation_boundary() {
    for protocol in [
        wire::ProtocolFamily::OpenAiResponses,
        wire::ProtocolFamily::GeminiInteractions,
    ] {
        let service = service(protocol);
        let continuation: wire::ContinuationRef = serde_json::from_value(json!({
            "protocol":protocol,"response_id":"parent-response","provider_id":"provider",
            "profile_name":"test","endpoint_fingerprint":"endpoint","account_scope":"account","request_model":"model"
        })).unwrap();
        scope_computer_request(
            Some(ComputerRequestProjection {
                native: None,
                continuation: Some(continuation.clone()),
                binding: None,
                submission: None,
            }),
            async {
                let messages = vec![user("early context"), user("summarize all context")];
                let request = auxiliary(&service, messages.clone(), vec![]);
                assert_independent(&request);
                let serialized = serde_json::to_string(&request.input).unwrap();
                assert!(serialized.contains("early context"));
                assert!(serialized.contains("summarize all context"));
                let strict = service
                    .build_json_schema_request_with_thinking(
                        "model",
                        Some("test"),
                        None,
                        messages,
                        json!({"type":"object"}),
                        Some(512),
                        None,
                        None,
                        None,
                        Some("classifier"),
                    )
                    .unwrap();
                assert_independent(&strict);
                assert!(serde_json::to_string(&strict.input)
                    .unwrap()
                    .contains("early context"));
                assert_eq!(current_continuation(), Some(continuation));
            },
        )
        .await;
    }
}

fn history_user(content: Vec<ContentBlock>) -> ConversationMessage {
    ConversationMessage::User { api_message_override: None,
        id: MessageId::new(),
        content,
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    }
}

fn gemini_native_history() -> (Vec<ConversationMessage>, wire::ContinuationRef) {
    let reference:wire::ContinuationRef=serde_json::from_value(json!({"protocol":"gemini_interactions","response_id":"parent-interaction","provider_id":"gemini","profile_name":"test","endpoint_fingerprint":"endpoint","account_scope":"account","request_model":"model"})).unwrap();
    let original=wire::ContentBlock::Native{value:wire::NativeExtension::new(lingxi_llm_client::providers::google::computer::CALL_FORMAT,json!({"type":"function_call","id":"native-call","name":"take_screenshot","arguments":{}})).unwrap()};
    let call = wire::decode_computer_calls(
        wire::NativeComputerProvider::Gemini,
        std::slice::from_ref(&original),
        Some(&reference),
        &wire::ComputerFrame {
            width: 800,
            height: 600,
            geometry_version: "frame".into(),
        },
    )
    .unwrap()
    .remove(0);
    let receipt=wire::encode_computer_receipt(&call,&wire::ComputerReceiptInput{results:vec![wire::NativeComputerResult{operation_index:0,status:wire::NativeExecutionStatus::Succeeded,content:"Observed desktop".into(),blocks:Some(vec![json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}})])}],acknowledged_safety_checks:vec![]}).unwrap();
    let source = "gemini_interactions";
    (
        vec![
            history_user(vec![ContentBlock::Text {
                text: "Inspect the desktop".into(),
                citations: None,
            }]),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                stop_reason: Some("tool_use".into()),
                content: vec![
                    ContentBlock::ToolUse { input_projection: None,
                        id: "synthetic".into(),
                        name: "computer".into(),
                        input: json!({"action":"screenshot"}),
                        provider_id: None,
                    },
                    ContentBlock::ProviderContent {
                        protocol: source.into(),
                        value: json!({"type":"lingxi_computer_binding","provider_response_id":"parent-interaction","call":call,"tool_use_ids":["synthetic"],"original_blocks":[original]}),
                    },
                    ContentBlock::ProviderContent {
                        protocol: source.into(),
                        value: json!({"type":"lingxi_computer_continuation","continuation":reference}),
                    },
                ],
            },
            history_user(vec![
                ContentBlock::ToolResult { output_projection: None,
                    tool_use_id: "synthetic".into(),
                    content: "Observed desktop".into(),
                    is_error: Some(false),
                    content_blocks: Some(vec![
                        json!({"type":"text","text":"Observed desktop"}),
                        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}}),
                    ]),
                    provider_tool_use_id: None,
                },
                ContentBlock::ProviderContent {
                    protocol: source.into(),
                    value: json!({"type":"lingxi_computer_receipt","provider_response_id":"parent-interaction","call_id":"native-call","block":receipt}),
                },
            ]),
            history_user(vec![ContentBlock::Text {
                text: "Summarize the earlier history".into(),
                citations: None,
            }]),
        ],
        reference,
    )
}

#[derive(Default)]
struct CaptureAuxiliary(std::sync::Mutex<Vec<serde_json::Value>>);
#[async_trait::async_trait]
impl crate::Transport for CaptureAuxiliary {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, wire::LlmError> {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap());
        Err(wire::LlmError::InvalidRequest {
            message: "fixture stopped after capture".into(),
        })
    }
}

#[tokio::test]
async fn auxiliary_dispatch_projects_actual_native_history_as_ordinary_tools() {
    for destination in [
        wire::ProtocolFamily::GeminiInteractions,
        wire::ProtocolFamily::OpenAiResponses,
        wire::ProtocolFamily::AnthropicMessages,
    ] {
        let capture = Arc::new(CaptureAuxiliary::default());
        let service = service_with_transport(destination, capture.clone());
        let (messages, continuation) = gemini_native_history();
        let journal = Arc::new(Journal::default());
        let submission = submission(journal.clone());
        journal
            .record_receipt(submission.receipts[0].clone())
            .await
            .unwrap();
        scope_computer_request(Some(ComputerRequestProjection {
            native: None, continuation: Some(continuation), binding: None, submission: Some(submission.clone()),
        }), async {
            let plain = auxiliary(&service, messages.clone(), vec![]);
            assert_independent(&plain);
            let strict = service.build_json_schema_request_with_thinking(
                "model", Some("test"), None, messages,
                json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
                Some(512), None, None, None, Some("compaction"),
            ).unwrap();
            assert_independent(&strict);
            let mut ready = vec![plain];
            if destination == wire::ProtocolFamily::GeminiInteractions {
                let error = service.execute_side_query_request(strict).await.unwrap_err();
                assert!(matches!(error, LlmError::UnsupportedCapability { .. }));
            } else {
                ready.push(strict);
            }
            let expected_sends = ready.len();
            for request in ready {
                let error = service.execute_side_query_request(request).await.unwrap_err();
                assert!(error.to_string().contains("fixture stopped after capture"), "{destination:?}: {error}");
            }
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), expected_sends);
            for request in requests.iter() {
                let body = request.to_string();
                assert!(body.contains("synthetic"), "{destination:?}: {body}");
                assert!(body.contains("Observed desktop"));
                assert!(body.contains("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="));
                assert!(body.contains("Inspect the desktop"));
                assert!(body.contains("Summarize the earlier history"));
                for excluded in ["native-call", "_sdk_continuation", "previous_response_id", "previous_interaction_id", "lingxi_computer"] {
                    assert!(!body.contains(excluded), "{destination:?}: auxiliary sent native authority {excluded}");
                }
            }
            assert_eq!(journal.receipt("receipt-1").await.unwrap().unwrap(), submission.receipts[0]);
        }).await;
    }
}
