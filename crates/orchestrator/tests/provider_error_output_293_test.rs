//! Replay the Native 2.1.293 HTTP error bodies through the production provider
//! adapter and both public orchestrator turn loops. This stops at the
//! `OutputStream`/session-JSONL boundary; the Harness checkout has no Native
//! `--print --output-format json` CLI serializer to exercise here.

use async_trait::async_trait;
use lingxi_core::host::{FileSystem, OutputEvent};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, SessionId};
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use serde_json::json;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-sonnet-4-5";
const PROMPT: &str = "Return the fixed provider error fixture response.";
const NATIVE_PREFIX: &str = "API Error: ";

struct FixedHttpError {
    status: u16,
    body: Vec<u8>,
    response_headers: Vec<(String, String)>,
    calls: AtomicUsize,
    request_shapes: Mutex<Vec<RequestShape>>,
    request_authorities: Mutex<Vec<Option<String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestShape {
    model: Option<String>,
    stream: Option<bool>,
}

fn request_shape(request: &lingxi_llm_client::HttpRequest) -> RequestShape {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(request.body.as_ref()).ok();
    RequestShape {
        model: parsed
            .as_ref()
            .and_then(|body| body.get("model"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        stream: parsed
            .as_ref()
            .and_then(|body| body.get("stream"))
            .and_then(serde_json::Value::as_bool),
    }
}

fn request_authority(request_url: &str) -> Option<String> {
    let parsed = url::Url::parse(request_url).ok()?;
    let host = parsed.host_str()?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Some(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

#[async_trait]
impl Transport for FixedHttpError {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.request_shapes
            .lock()
            .expect("request-shape mutex")
            .push(request_shape(&request));
        self.request_authorities
            .lock()
            .expect("request-authority mutex")
            .push(request_authority(&request.url));
        let mut headers = vec![("content-type".into(), "application/json".into())];
        headers.extend(self.response_headers.iter().cloned());
        Ok(lingxi_llm_client::HttpResponse {
            status: self.status,
            headers,
            body: self.body.clone().into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<FixedHttpError>) -> Arc<ApiService> {
    api_service_with_base_url(transport, "https://api.anthropic.com")
}

fn api_service_with_base_url(transport: Arc<FixedHttpError>, base_url: &str) -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": base_url,
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": false,
                    "structured_output": false
                }
            }]
        }]
    }))
    .expect("fixed provider config");

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "current-alignment-provider-error",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled),
    )
}

fn native_result_units(baseline: &std::path::Path, case: &str) -> Vec<u16> {
    let raw = std::fs::read_to_string(baseline.join(case).join("native.stdout.json.raw"))
        .expect("checked-in raw Native JSON stdout");
    Utf16JsonProjection::parse(&raw)
        .expect("Native JSON parses while retaining escaped lone surrogates")
        .string_units("/result")
        .expect("Native result text")
}

fn native_status_result_units(baseline: &std::path::Path, status: u16) -> Vec<u16> {
    let raw = std::fs::read_to_string(
        baseline
            .join("status-controls")
            .join(format!("http-{status}.stdout.raw")),
    )
    .expect("checked-in raw Native status-control JSON stdout");
    Utf16JsonProjection::parse(&raw)
        .expect("Native JSON parses while retaining escaped lone surrogates")
        .string_units("/result")
        .expect("Native status-control result text")
}

fn native_retry_header_result_units(baseline: &std::path::Path, status: u16) -> Vec<u16> {
    let raw = std::fs::read_to_string(
        baseline
            .join("retry-header-controls")
            .join(format!("http-{status}-no-retry-header.stdout.raw")),
    )
    .expect("checked-in Native retry-header JSON stdout");
    Utf16JsonProjection::parse(&raw)
        .expect("Native retry-header JSON parses with source units")
        .string_units("/result")
        .expect("Native retry-header result text")
}

fn native_retry_header_gateway_authority(baseline: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(baseline.join("retry-header-controls").join("receipt.json"))
        .expect("Native retry-header receipt");
    serde_json::from_str::<serde_json::Value>(&raw).expect("valid retry-header receipt")
        ["native_500_gateway_authority"]
        .as_str()
        .expect("fixed Native 500 gateway authority")
        .to_owned()
}

fn native_status_request_shapes(
    baseline: &std::path::Path,
    status: u16,
) -> Option<Vec<RequestShape>> {
    let fixture_path = baseline.parent()?.join("status-controls.json");
    let fixture = std::fs::read_to_string(fixture_path).expect("fixed Native status fixture");
    let fixture: serde_json::Value =
        serde_json::from_str(&fixture).expect("valid status fixture JSON");
    let case = fixture["cases"]
        .as_array()?
        .iter()
        .find(|case| case["http_status"].as_u64() == Some(status.into()))?;
    let expected = case.get("native_request_shape")?;
    if expected.is_null() {
        return None;
    }
    let model = expected["model"].as_str()?.to_owned();
    let streams = expected["stream_sequence"].as_array()?;
    Some(
        streams
            .iter()
            .map(|stream| RequestShape {
                model: Some(model.clone()),
                stream: stream.as_bool(),
            })
            .collect(),
    )
}

fn fixed_session_id_with(id: &str) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(id).expect("fixed test session id"))
}

fn fixed_session_id() -> SessionId {
    fixed_session_id_with("00000000-0000-4000-8000-000000002942")
}

#[tokio::test]
async fn native_http_error_bodies_reach_real_orchestrator_output_and_jsonl() {
    let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/provider-error/native-baseline");
    let cases = [
        "nested-error-message-lone-surrogate",
        "empty-response-body",
        "nested-error-message-null",
    ];
    let native_prefix = NATIVE_PREFIX.encode_utf16().collect::<Vec<_>>();
    for case in cases {
        let case_root = fixture_root.join(case);
        let response_body = std::fs::read(case_root.join("http-response.body.raw"))
            .expect("captured Native response body");
        let result_units = native_result_units(&fixture_root, case);
        assert!(
            result_units.starts_with(&native_prefix),
            "fixture {case} must retain Native's outer CLI prefix"
        );
        // The SDK/Host source test closes the inner provider-text seam. The
        // Native persisted assistant row confirms the formatted user-visible
        // text also contains this outer prefix, so compare the full text at
        // the production output and transcript boundaries.
        let expected_native_text_units = result_units.clone();
        for streaming in [false, true] {
            let dir = tempdir().expect("isolated transcript directory");
            let transcript_path = dir.path().join("session.jsonl");
            let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
            let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));
            let transport = Arc::new(FixedHttpError {
                status: 400,
                body: response_body.clone(),
                response_headers: Vec::new(),
                calls: AtomicUsize::new(0),
                request_shapes: Mutex::new(Vec::new()),
                request_authorities: Mutex::new(Vec::new()),
            });
            let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
            let output = Arc::new(MockOutputStream::new());
            let orchestrator = ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: MODEL.to_owned(),
                    max_turns: 1,
                    system_prompt_override: Some("Fixed provider error test prompt.".into()),
                    ..OrchestratorConfig::default()
                },
                adapter.clone(),
                adapter,
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_session_id(fixed_session_id())
            .with_jsonl_writer(writer);

            let outcome = if streaming {
                orchestrator.run_turn_streaming(PROMPT).await
            } else {
                orchestrator.run_turn(PROMPT).await
            }
            .expect("generic HTTP 400 is surfaced as a completed model-error turn");
            assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
            assert_eq!(
                transport.calls.load(Ordering::SeqCst),
                1,
                "{case} streaming={streaming}"
            );

            let events = output.snapshot().await;
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    OutputEvent::TerminalApiError {
                        api_error_status: Some(400), terminal_reason,
                    } if terminal_reason == "api_error"
                )),
                "typed terminal API status/reason must survive the output boundary: {case} streaming={streaming}: {events:#?}"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
                )),
                "{case} streaming={streaming}: {events:#?}"
            );
            let text_events = events
                .iter()
                .filter(|event| matches!(event, OutputEvent::Text { .. }))
                .collect::<Vec<_>>();
            assert_eq!(
                text_events.len(),
                1,
                "{case} streaming={streaming}: {events:#?}"
            );
            let event = text_events[0];
            let OutputEvent::Text {
                text: actual_output_source,
            } = event
            else {
                unreachable!("filtered OutputEvent::Text")
            };
            let actual_output_units = actual_output_source
                .string_units("")
                .expect("OutputEvent keeps the exact source UTF-16 units");
            assert_eq!(
                actual_output_units, expected_native_text_units,
                "the actual output event must preserve the complete Native user-visible text: {case} streaming={streaming}"
            );

            // Capture and round-trip the real OutputStream event serializer.
            // This compares the complete Native text including its prefix;
            // the CLI's outer result envelope remains a separate gate.
            let serialized_event = serde_json::to_string(event).expect("OutputEvent serializer");
            let serialized_projection = Utf16JsonProjection::parse(&serialized_event)
                .expect("serialized event is exact JSON");
            assert_eq!(
                serialized_projection
                    .string_units("/Text/text")
                    .expect("serialized Text payload"),
                expected_native_text_units,
                "serialized OutputEvent retains the complete Native text units: {case} streaming={streaming}"
            );
            let decoded_event: OutputEvent =
                serde_json::from_str(&serialized_event).expect("OutputEvent wire round-trip");
            assert_eq!(&decoded_event, event, "{case} streaming={streaming}");

            let rows = JsonlReader::new(transcript_path, fs)
                .read_all()
                .await
                .expect("read actual runtime session JSONL");
            let assistant_rows = rows
                .iter()
                .filter(|row| row.message_type == "assistant")
                .collect::<Vec<_>>();
            assert_eq!(assistant_rows.len(), 1, "{case} streaming={streaming}");
            let transcript_units = assistant_rows[0]
                .message
                .string_units("/content/0/text")
                .expect("assistant text in source-preserving JSONL");
            assert_eq!(
                transcript_units, expected_native_text_units,
                "the actual session row must retain the complete Native text units: {case} streaming={streaming}"
            );
            assert_eq!(
                assistant_rows[0].message.string_at("/stop_reason"),
                Some("model_error"),
                "{case} streaming={streaming}"
            );
        }
    }
}

#[tokio::test]
async fn native_http_status_controls_reach_real_orchestrator_output() {
    let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/provider-error/native-baseline");
    let response_body = std::fs::read(
        fixture_root
            .join("nested-error-message-lone-surrogate")
            .join("http-response.body.raw"),
    )
    .expect("captured fixed response body shared by status controls");

    for (status, session_id) in [
        (403, "00000000-0000-4000-8000-000000002952"),
        (404, "00000000-0000-4000-8000-000000002953"),
        (413, "00000000-0000-4000-8000-000000002954"),
    ] {
        let expected_units = native_status_result_units(&fixture_root, status);
        for streaming in [false, true] {
            let dir = tempdir().expect("isolated transcript directory");
            let transport = Arc::new(FixedHttpError {
                status,
                body: response_body.clone(),
                response_headers: Vec::new(),
                calls: AtomicUsize::new(0),
                request_shapes: Mutex::new(Vec::new()),
                request_authorities: Mutex::new(Vec::new()),
            });
            let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
            let output = Arc::new(MockOutputStream::new());
            let orchestrator = ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: MODEL.to_owned(),
                    max_turns: 1,
                    system_prompt_override: Some("Fixed provider status test prompt.".into()),
                    ..OrchestratorConfig::default()
                },
                adapter.clone(),
                adapter,
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_session_id(fixed_session_id_with(session_id));

            let outcome = if streaming {
                orchestrator.run_turn_streaming(PROMPT).await
            } else {
                orchestrator.run_turn(PROMPT).await
            }
            .expect("fixed HTTP status is surfaced as a completed error turn");
            assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
            let request_shapes = transport
                .request_shapes
                .lock()
                .expect("request-shape mutex")
                .clone();
            if streaming {
                if let Some(expected_native_shapes) =
                    native_status_request_shapes(&fixture_root, status)
                {
                    assert_eq!(
                        request_shapes, expected_native_shapes,
                        "match Native model/stream attempt chain, not only its request count: status={status}"
                    );
                }
            }

            let events = output.snapshot().await;
            let expected_terminal_reason = if status == 413 {
                "image_error"
            } else {
                "api_error"
            };
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    OutputEvent::TerminalApiError { api_error_status: Some(actual), terminal_reason }
                        if *actual == status && terminal_reason == expected_terminal_reason
                )),
                "typed terminal API status/reason must survive the output boundary: status={status} streaming={streaming}: {events:#?}"
            );
            let text_events = events
                .iter()
                .filter(|event| matches!(event, OutputEvent::Text { .. }))
                .collect::<Vec<_>>();
            assert_eq!(
                text_events.len(),
                1,
                "status={status} streaming={streaming}: {events:#?}"
            );
            let OutputEvent::Text { text } = text_events[0] else {
                unreachable!("filtered OutputEvent::Text")
            };
            assert_eq!(
                text.string_units("").expect("exact output text units"),
                expected_units,
                "complete Native status-specific presentation: status={status} streaming={streaming}"
            );
        }
    }
}

#[tokio::test]
async fn native_retry_header_401_and_500_reach_real_orchestrator_output() {
    let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/provider-error/native-baseline");
    let response_body = std::fs::read(
        fixture_root
            .join("nested-error-message-lone-surrogate")
            .join("http-response.body.raw"),
    )
    .expect("captured fixed Native nested-error body");
    let gateway_authority = native_retry_header_gateway_authority(&fixture_root);

    for (status, session_id) in [
        (401, "00000000-0000-4000-8000-000000002961"),
        (500, "00000000-0000-4000-8000-000000002962"),
    ] {
        let expected_native_text_units = native_retry_header_result_units(&fixture_root, status);
        let base_url = if status == 500 {
            // This fixed authority comes from the checked-in Native 500 JSON
            // baseline. The test injects a Transport and never contacts it.
            format!("http://{gateway_authority}")
        } else {
            "https://api.anthropic.com".to_owned()
        };
        for streaming in [false, true] {
            let dir = tempdir().expect("isolated transcript directory");
            let transcript_path = dir.path().join("session.jsonl");
            let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
            let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));
            let transport = Arc::new(FixedHttpError {
                status,
                body: response_body.clone(),
                response_headers: vec![("x-should-retry".into(), "false".into())],
                calls: AtomicUsize::new(0),
                request_shapes: Mutex::new(Vec::new()),
                request_authorities: Mutex::new(Vec::new()),
            });
            let adapter = Arc::new(ProviderApiAdapter::new(api_service_with_base_url(
                transport.clone(),
                &base_url,
            )));
            let output = Arc::new(MockOutputStream::new());
            let orchestrator = ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: MODEL.to_owned(),
                    max_turns: 1,
                    system_prompt_override: Some("Fixed retry-header status test prompt.".into()),
                    ..OrchestratorConfig::default()
                },
                adapter.clone(),
                adapter,
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_session_id(fixed_session_id_with(session_id))
            .with_jsonl_writer(writer);

            let outcome = if streaming {
                orchestrator.run_turn_streaming(PROMPT).await
            } else {
                orchestrator.run_turn(PROMPT).await
            }
            .expect("explicit retry-header status is surfaced as a completed error turn");
            assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
            assert_eq!(
                transport.calls.load(Ordering::SeqCst),
                1,
                "x-should-retry:false makes status {status} terminal; streaming={streaming}"
            );
            let authorities = transport
                .request_authorities
                .lock()
                .expect("request-authority mutex")
                .clone();
            let expected_authority = if status == 500 {
                gateway_authority.as_str()
            } else {
                "api.anthropic.com"
            };
            assert_eq!(
                authorities,
                vec![Some(expected_authority.to_owned())],
                "actual dispatch authority selects Native first-party/gateway presentation: status={status} streaming={streaming}"
            );

            let events = output.snapshot().await;
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    OutputEvent::TerminalApiError { api_error_status: Some(actual), terminal_reason }
                        if *actual == status && terminal_reason == "api_error"
                )),
                "typed retry-header status must reach output: status={status} streaming={streaming}: {events:#?}"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
                )),
                "status={status} streaming={streaming}: {events:#?}"
            );
            let text_events = events
                .iter()
                .filter(|event| matches!(event, OutputEvent::Text { .. }))
                .collect::<Vec<_>>();
            assert_eq!(
                text_events.len(),
                1,
                "status={status} streaming={streaming}"
            );
            let OutputEvent::Text { text } = text_events[0] else {
                unreachable!("filtered OutputEvent::Text")
            };
            let actual_units = text
                .string_units("")
                .expect("output event retains exact source units");
            assert_eq!(
                actual_units, expected_native_text_units,
                "complete Native retry-header result without stripping the prefix: status={status} streaming={streaming}"
            );

            let serialized = serde_json::to_string(text_events[0]).expect("text event serializer");
            let serialized_projection =
                Utf16JsonProjection::parse(&serialized).expect("serialized output event JSON");
            assert_eq!(
                serialized_projection.string_units("/Text/text"),
                Some(expected_native_text_units.clone()),
                "serialized text event preserves the complete Native message: status={status} streaming={streaming}"
            );

            let rows = JsonlReader::new(transcript_path, fs)
                .read_all()
                .await
                .expect("read actual runtime session JSONL");
            let assistant_rows = rows
                .iter()
                .filter(|row| row.message_type == "assistant")
                .collect::<Vec<_>>();
            assert_eq!(
                assistant_rows.len(),
                1,
                "status={status} streaming={streaming}"
            );
            assert_eq!(
                assistant_rows[0]
                    .message
                    .string_units("/content/0/text")
                    .expect("assistant source text in session JSONL"),
                expected_native_text_units,
                "persisted row retains the complete Native retry-header text: status={status} streaming={streaming}"
            );
        }
    }
}
