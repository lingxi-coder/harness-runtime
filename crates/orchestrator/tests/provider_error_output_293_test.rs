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
use sha2::{Digest, Sha256};
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

fn native_result_stop_reason(baseline: &std::path::Path, case: &str) -> String {
    let raw = std::fs::read_to_string(baseline.join(case).join("native.stdout.json.raw"))
        .expect("checked-in raw Native JSON stdout");
    Utf16JsonProjection::parse(&raw)
        .expect("Native JSON parses while retaining escaped lone surrogates")
        .string_at("/stop_reason")
        .expect("Native stop reason")
        .to_owned()
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
        let expected_native_stop_reason = native_result_stop_reason(&fixture_root, case);
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
            let orchestrator = ConversationOrchestrator::into_shared(
                ConversationOrchestrator::new_with_streaming(
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
                .with_jsonl_writer(writer),
            );

            let outcome = if streaming {
                orchestrator.run_turn_streaming(PROMPT).await
            } else {
                orchestrator.run_turn(PROMPT).await
            }
            .expect("generic HTTP 400 is surfaced as a completed model-error turn");
            assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
            let native_result = Utf16JsonProjection::parse(
                &std::fs::read_to_string(case_root.join("native.stdout.json.raw"))
                    .expect("original Native result envelope"),
            )
            .expect("source-preserving Native envelope");
            let metrics = orchestrator
                .completed_turn_metrics()
                .expect("completed error turn publishes its own execution metrics");
            assert_eq!(
                Some(f64::from(metrics.num_turns)),
                native_result
                    .get_property("num_turns")
                    .and_then(|value| value.as_number()),
                "failed requests still count their admitted query: {case} streaming={streaming}"
            );
            assert_eq!(metrics.api_error_status, Some(400));
            assert_eq!(
                metrics.api_error_stop_reason.as_deref(),
                Some("stop_sequence")
            );
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
                        api_error_status: Some(400), terminal_reason, stop_reason,
                    } if terminal_reason == "api_error" && stop_reason == "stop_sequence"
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
                Some(expected_native_stop_reason.as_str()),
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
            let orchestrator = ConversationOrchestrator::into_shared(
                ConversationOrchestrator::new_with_streaming(
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
                .with_session_id(fixed_session_id_with(session_id)),
            );

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
                    OutputEvent::TerminalApiError { api_error_status: Some(actual), terminal_reason, stop_reason }
                        if *actual == status && terminal_reason == expected_terminal_reason && stop_reason == "stop_sequence"
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
            let orchestrator = ConversationOrchestrator::into_shared(
                ConversationOrchestrator::new_with_streaming(
                    OrchestratorConfig {
                        model: MODEL.to_owned(),
                        max_turns: 1,
                        system_prompt_override: Some(
                            "Fixed retry-header status test prompt.".into(),
                        ),
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
                .with_jsonl_writer(writer),
            );

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
                    OutputEvent::TerminalApiError { api_error_status: Some(actual), terminal_reason, stop_reason }
                        if *actual == status && terminal_reason == "api_error" && stop_reason == "stop_sequence"
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

fn fixture_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn native_295_http_422_capture_reaches_real_orchestrator_output_and_jsonl() {
    let capture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/provider-error-295/native-baseline/2.1.295/http-422");
    let case_raw =
        std::fs::read(capture_dir.join("capture.json")).expect("Native 422 capture receipt");
    let case: serde_json::Value = serde_json::from_slice(&case_raw).expect("valid case receipt");
    let manifest_raw =
        std::fs::read(capture_dir.join("capture-manifest.json")).expect("capture runner manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_raw).expect("valid manifest");
    let runner_raw =
        std::fs::read(capture_dir.join("capture-runner.py")).expect("captured runner source");
    assert_eq!(case["phase"], "complete");
    assert_eq!(case["capture_complete"], true);
    assert_eq!(case["native_process_launches"], 1);
    assert_eq!(case["messages_request_count"], 1);
    assert_eq!(case["native_exit_code"], 1);
    assert_eq!(case["timed_out"], false);
    assert_eq!(
        case["native_sha256_actual"],
        "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
    );
    assert_eq!(manifest["target_native_version"], "2.1.295");
    assert_eq!(
        manifest["target_native_sha256"],
        case["native_sha256_actual"]
    );
    assert_eq!(
        fixture_sha256(&runner_raw),
        case["runner_sha256"].as_str().expect("runner hash")
    );
    assert_eq!(case["runner_sha256"], manifest["runner_sha256"]);
    assert_eq!(
        case["runner_relative_path"],
        manifest["runner_relative_path"]
    );
    assert_eq!(
        fixture_sha256(&manifest_raw),
        case["runner_manifest_sha256"]
            .as_str()
            .expect("runner manifest hash")
    );

    let requests_raw = std::fs::read(capture_dir.join("requests.json")).expect("request receipt");
    let requests: Vec<serde_json::Value> =
        serde_json::from_slice(&requests_raw).expect("valid requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request["peer"], "127.0.0.1");
    assert_eq!(request["method"], "POST");
    assert_eq!(request["target"], "/v1/messages?beta=true");
    assert_eq!(request["status"], 422);
    assert_eq!(request["model"], MODEL);
    assert_eq!(request["x_api_key_is_dummy"], true);

    let request_bytes = std::fs::read(capture_dir.join("request-000.raw")).expect("raw request");
    let request_headers =
        std::fs::read(capture_dir.join("request-000-headers.raw")).expect("raw request headers");
    let request_body =
        std::fs::read(capture_dir.join("request-000-body.raw")).expect("raw request body");
    assert_eq!(
        request_bytes,
        [request_headers.as_slice(), request_body.as_slice()].concat()
    );
    assert_eq!(
        fixture_sha256(&request_bytes),
        "86532df92c00790f148eb1772f3f22db411b141898c7e5daa3bd547253d24037"
    );
    assert_eq!(
        fixture_sha256(&request_headers),
        "f6d629fb7452d22b1ced18ae951295693c9c31afad6327ed3be308d42d9b4cca"
    );
    assert_eq!(
        fixture_sha256(&request_body),
        "aa027db7c377ef31b0eb26836028aa544b3e7ca8805e7cc57c49f3a85d4daa9f"
    );
    assert_eq!(
        fixture_sha256(&request_bytes),
        request["request_sha256"]
            .as_str()
            .expect("raw request hash")
    );
    assert_eq!(
        fixture_sha256(&request_headers),
        request["headers_sha256"].as_str().expect("raw header hash")
    );
    assert_eq!(
        fixture_sha256(&request_body),
        request["body_sha256"].as_str().expect("raw body hash")
    );
    let request_json: serde_json::Value =
        serde_json::from_slice(&request_body).expect("captured request JSON");
    assert_eq!(request_json["model"], MODEL);
    assert_eq!(request_json["stream"], true);
    let prompt = std::fs::read(capture_dir.join("invocation.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .and_then(|invocation| invocation["prompt"].as_str().map(ToOwned::to_owned))
        .expect("fixed captured prompt");
    assert_eq!(request_json["messages"][0]["content"][0]["text"], prompt);
    let captured_shape = RequestShape {
        model: Some(MODEL.to_owned()),
        stream: Some(true),
    };

    let response_bytes = std::fs::read(capture_dir.join("response-000.raw")).expect("raw response");
    let response_body =
        std::fs::read(capture_dir.join("response-000-body.raw")).expect("raw response body");
    assert!(response_bytes.starts_with(b"HTTP/1.1 422 Unprocessable Entity\r\n"));
    assert_eq!(
        response_body.as_slice(),
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"NATIVE_295_PRINT_HTTP_422_ERROR_COPY_PROBE"}}"#
    );
    assert_eq!(
        fixture_sha256(&response_bytes),
        "f572e57d8def4499ff96b437385b911ecd6d904b50dd2b0202f7cfb7a03bbe82"
    );
    assert_eq!(
        fixture_sha256(&response_body),
        "2f92d0946ef4a3e49cfecd806411010044670dbccfa2edc80ca0e6e3bfa1129a"
    );
    assert_eq!(
        fixture_sha256(&response_bytes),
        request["response_sha256"]
            .as_str()
            .expect("raw response hash")
    );
    assert_eq!(
        fixture_sha256(&response_body),
        request["response_body_sha256"]
            .as_str()
            .expect("raw response body hash")
    );

    let stdout_raw =
        std::fs::read(capture_dir.join("native.stdout.raw")).expect("raw Native stdout");
    let stderr_raw =
        std::fs::read(capture_dir.join("native.stderr.raw")).expect("raw Native stderr");
    assert_eq!(
        fixture_sha256(&stdout_raw),
        "aefe021e6303428b141ae07387783b7b2dad3538afee85332eec1cb8cb2a350c"
    );
    assert_eq!(
        fixture_sha256(&stderr_raw),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let exit_raw =
        std::fs::read(capture_dir.join("native-exit.json")).expect("Native exit receipt");
    let exit: serde_json::Value = serde_json::from_slice(&exit_raw).expect("valid exit receipt");
    assert_eq!(exit["returncode"], 1);
    assert_eq!(exit["timed_out"], false);
    assert_eq!(exit["native_process_launches"], 1);
    assert_eq!(
        exit["stdout_sha256"].as_str().expect("stdout hash"),
        fixture_sha256(&stdout_raw)
    );
    assert_eq!(
        exit["stderr_sha256"].as_str().expect("stderr hash"),
        fixture_sha256(&stderr_raw)
    );
    let native_stdout = std::str::from_utf8(&stdout_raw).expect("captured stdout is UTF-8 JSON");
    let native_envelope: serde_json::Value =
        serde_json::from_slice(&stdout_raw).expect("Native result JSON");
    assert_eq!(native_envelope["is_error"], true);
    assert_eq!(native_envelope["api_error_status"], 422);
    assert_eq!(native_envelope["terminal_reason"], "api_error");
    let native_result_units = Utf16JsonProjection::parse(native_stdout)
        .expect("Native result JSON retains source units")
        .string_units("/result")
        .expect("Native result text");
    assert_eq!(
        String::from_utf16_lossy(&native_result_units),
        "API Error: 422 NATIVE_295_PRINT_HTTP_422_ERROR_COPY_PROBE"
    );

    for streaming in [false, true] {
        let dir = tempdir().expect("isolated transcript directory");
        let transcript_path = dir.path().join("session.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));
        let transport = Arc::new(FixedHttpError {
            status: 422,
            body: response_body.clone(),
            response_headers: Vec::new(),
            calls: AtomicUsize::new(0),
            request_shapes: Mutex::new(Vec::new()),
            request_authorities: Mutex::new(Vec::new()),
        });
        let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
        let output = Arc::new(MockOutputStream::new());
        let orchestrator = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: MODEL.to_owned(),
                    max_turns: 1,
                    system_prompt_override: Some(
                        "You are a local deterministic headless fixture.".into(),
                    ),
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
            .with_session_id(fixed_session_id_with(
                case["session_id"].as_str().expect("session id"),
            ))
            .with_jsonl_writer(writer),
        );
        let outcome = if streaming {
            orchestrator.run_turn_streaming(&prompt).await
        } else {
            orchestrator.run_turn(&prompt).await
        }
        .expect("captured HTTP 422 is a completed model-error turn");
        assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            1,
            "streaming={streaming}"
        );
        if streaming {
            assert_eq!(
                transport
                    .request_shapes
                    .lock()
                    .expect("request-shape mutex")
                    .as_slice(),
                std::slice::from_ref(&captured_shape),
                "the live streaming dispatch retains the captured model and stream setting"
            );
        }
        let metrics = orchestrator
            .completed_turn_metrics()
            .expect("completed 422 turn metrics");
        assert_eq!(metrics.api_error_status, Some(422));
        assert_eq!(
            metrics.api_error_stop_reason.as_deref(),
            Some("stop_sequence")
        );

        let events = output.snapshot().await;
        assert!(events.iter().any(|event| matches!(
            event,
            OutputEvent::TerminalApiError { api_error_status: Some(422), terminal_reason, stop_reason }
                if terminal_reason == "api_error" && stop_reason == "stop_sequence"
        )), "terminal 422 metadata must survive the production output path: streaming={streaming}: {events:#?}");
        let text_events = events
            .iter()
            .filter(|event| matches!(event, OutputEvent::Text { .. }))
            .collect::<Vec<_>>();
        assert_eq!(text_events.len(), 1, "streaming={streaming}: {events:#?}");
        let OutputEvent::Text { text } = text_events[0] else {
            unreachable!("filtered Text event")
        };
        assert_eq!(
            text.string_units("").expect("exact output text units"),
            native_result_units,
            "the production output text must match Native's captured HTTP 422 result: streaming={streaming}"
        );
        let serialized = serde_json::to_string(text_events[0]).expect("Text event serialization");
        let serialized_projection =
            Utf16JsonProjection::parse(&serialized).expect("serialized event source JSON");
        assert_eq!(
            serialized_projection.string_units("/Text/text"),
            Some(native_result_units.clone())
        );

        let rows = JsonlReader::new(transcript_path, fs)
            .read_all()
            .await
            .expect("session JSONL replay");
        let assistant_rows = rows
            .iter()
            .filter(|row| row.message_type == "assistant")
            .collect::<Vec<_>>();
        assert_eq!(assistant_rows.len(), 1, "streaming={streaming}");
        assert_eq!(
            assistant_rows[0].message.string_units("/content/0/text").expect("assistant text source"),
            native_result_units,
            "persisted assistant text must match Native's captured 422 result: streaming={streaming}"
        );
    }
}
