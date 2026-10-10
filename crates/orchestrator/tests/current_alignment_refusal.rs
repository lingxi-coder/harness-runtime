//! Replay the pinned Native 2.1.295 refusal stop-details cases through the
//! production SDK -> provider adapter -> orchestrator -> OutputStream/JSONL
//! path. This test intentionally compares the full Native CLI result and the
//! source-preserving persisted stop_details field; it does not use a mock
//! terminal formatter or construct the result text itself.

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
use std::future::Future;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-sonnet-4-5";

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("current-alignment-refusal".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build refusal-test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack refusal test thread");
    handle.join().expect("refusal test thread panicked");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestShape {
    model: Option<String>,
    stream: Option<bool>,
}

struct FixedNativeRefusal {
    response_body: Vec<u8>,
    request_shapes: Mutex<Vec<RequestShape>>,
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

#[async_trait]
impl Transport for FixedNativeRefusal {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.request_shapes
            .lock()
            .expect("request-shape mutex")
            .push(request_shape(&request));
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: self.response_body.clone().into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<FixedNativeRefusal>) -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": "https://api.anthropic.com",
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
            "current-alignment-native-refusal",
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

fn fixed_session_id(id: &str) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(id).expect("fixed test session id"))
}

fn result_units(baseline_case: &std::path::Path) -> Vec<u16> {
    let raw = std::fs::read(baseline_case.join("native.stdout.raw"))
        .expect("captured original Native stdout bytes");
    Utf16JsonProjection::parse(&String::from_utf8(raw).expect("Native stdout is UTF-8 JSON"))
        .expect("Native JSON parses without reserializing")
        .string_units("/result")
        .expect("Native result text")
}

fn verify_fixture_checksums(fixture_root: &std::path::Path) {
    let manifest = std::fs::read_to_string(fixture_root.join("SHA256SUMS"))
        .expect("pinned Native 2.1.295 fixture checksums");
    let mut checked = 0;
    for line in manifest.lines() {
        let (expected, relative_path) = line
            .split_once("  ")
            .expect("sha256sum entry uses two-space separator");
        let relative_path = std::path::Path::new(relative_path);
        assert!(
            !relative_path.is_absolute(),
            "fixture checksum path must stay relative: {relative_path:?}"
        );
        let raw = std::fs::read(fixture_root.join(relative_path))
            .unwrap_or_else(|error| panic!("read fixture artifact {relative_path:?}: {error}"));
        let actual = format!("{:x}", Sha256::digest(raw));
        assert_eq!(
            actual, expected,
            "fixture artifact changed: {relative_path:?}"
        );
        checked += 1;
    }
    assert!(checked > 0, "fixture checksum manifest must not be empty");
}

fn verify_native_295_receipt(
    baseline_case: &std::path::Path,
    case: &str,
    session_id: &str,
    response_id: &str,
    stdout_result_units: &[u16],
) {
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(baseline_case.join("native-receipt.json"))
            .expect("captured Native receipt bytes"),
    )
    .expect("valid Native receipt JSON");
    assert_eq!(
        receipt["native_version"].as_str(),
        Some("2.1.295 (Claude Code)")
    );
    assert_eq!(
        receipt["native_sha256"].as_str(),
        Some("0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70")
    );
    assert_eq!(receipt["case_id"].as_str(), Some(case));
    assert_eq!(receipt["fixed_inputs"]["model"].as_str(), Some(MODEL));
    assert_eq!(
        receipt["fixed_inputs"]["session_id"].as_str(),
        Some(session_id)
    );
    assert_eq!(
        receipt["fixed_inputs"]["response_id"].as_str(),
        Some(response_id)
    );
    assert_eq!(receipt["native_exit_code"].as_i64(), Some(1));
    assert_eq!(receipt["timed_out"].as_bool(), Some(false));

    let receipt_result_units: Vec<u16> = serde_json::from_value(
        receipt["stop_details_observation"]["native_cli_result_utf16_units"].clone(),
    )
    .expect("receipt contains the raw Native result UTF-16 units");
    assert_eq!(receipt_result_units.as_slice(), stdout_result_units);
    assert_eq!(
        receipt["stop_details_observation"]["native_session_stop_details"]
            .as_array()
            .map(Vec::len),
        Some(3),
        "Native 2.1.295 persisted three stop-details rows: {case}"
    );
}

fn explanation_units_hex(fixture_root: &std::path::Path) -> Vec<u16> {
    let raw =
        std::fs::read_to_string(fixture_root.join("cases.json")).expect("fixed stop-details cases");
    let cases: serde_json::Value = serde_json::from_str(&raw).expect("valid cases JSON");
    let hex = cases["explanation_utf16_units_hex"]
        .as_str()
        .expect("fixed explanation UTF-16 hex");
    hex.as_bytes()
        .chunks_exact(4)
        .map(|chunk| {
            u16::from_str_radix(std::str::from_utf8(chunk).expect("ASCII UTF-16 hex"), 16)
                .expect("valid UTF-16 code unit")
        })
        .collect()
}

#[test]
fn native_refusal_results_and_source_stop_details_reach_real_host_output_and_jsonl() {
    run_with_large_stack(
        native_refusal_results_and_source_stop_details_reach_real_host_output_and_jsonl_inner,
    );
}

async fn native_refusal_results_and_source_stop_details_reach_real_host_output_and_jsonl_inner() {
    let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
    verify_fixture_checksums(&fixture_root);
    let explanation_units = explanation_units_hex(&fixture_root);
    let cases = [
        (
            "refusal-cyber",
            "00000000-0000-4000-8000-000000002950",
            "cyber",
            "msg_fixed_stop_details_cyber_293",
        ),
        (
            "refusal-generic",
            "00000000-0000-4000-8000-000000002951",
            "",
            "msg_fixed_stop_details_generic_293",
        ),
    ];

    for (case, session_id, category, response_id) in cases {
        let baseline = fixture_root.join("native-baseline").join(case);
        let response_body = std::fs::read(baseline.join("native-response-01.sse.raw"))
            .expect("original Native SSE response bytes");
        let expected_result = result_units(&baseline);
        verify_native_295_receipt(&baseline, case, session_id, response_id, &expected_result);
        let expected_result_text =
            String::from_utf16(&expected_result).expect("Native CLI refusal result is scalar text");
        assert!(
            response_body
                .windows(response_id.len())
                .any(|window| window == response_id.as_bytes()),
            "captured provider response must carry the response ID named by Native output: {case}"
        );
        assert!(
            expected_result_text.ends_with(&format!("\n\nMessage ID: {response_id}")),
            "Native result must be linked to the fixed provider response id: {case}"
        );

        let dir = tempdir().expect("isolated transcript directory");
        let transcript_path = dir.path().join("session.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));
        let transport = Arc::new(FixedNativeRefusal {
            response_body,
            request_shapes: Mutex::new(Vec::new()),
        });
        let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
        let output = Arc::new(MockOutputStream::new());
        let orchestrator = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 1,
                system_prompt_override: Some("Fixed Native refusal source test prompt.".into()),
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
        .with_session_id(fixed_session_id(session_id))
        .with_jsonl_writer(writer);
        let orchestrator = ConversationOrchestrator::into_shared(orchestrator);

        let outcome = orchestrator
            .run_turn_streaming("Return the fixed refusal stop-details fixture response.")
            .await
            .expect("fixed refusal reaches the terminal turn boundary");
        assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
        let metrics = orchestrator
            .completed_turn_metrics()
            .expect("refusal publishes completed execution metrics");
        assert_eq!(metrics.num_turns, 1);
        assert_eq!(metrics.api_error_status, None);
        assert_eq!(metrics.api_error_stop_reason.as_deref(), Some("refusal"));

        let request_shapes = transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert!(
            !request_shapes.is_empty()
                && request_shapes
                    .iter()
                    .all(|shape| shape.model.as_deref() == Some(MODEL) && shape.stream == Some(true)),
            "the production adapter must replay the fixed Native streaming refusal: {case}: {request_shapes:?}"
        );

        let events = output.snapshot().await;
        let terminal_api_errors = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::TerminalApiError {
                    api_error_status,
                    terminal_reason,
                    stop_reason,
                } => Some((
                    *api_error_status,
                    terminal_reason.as_str(),
                    stop_reason.as_str(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            terminal_api_errors,
            vec![(None, "api_error", "refusal")],
            "the Host terminal marker must match the Native refusal envelope: {case}"
        );
        let text_events = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::Text { text } => text.string_units(""),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            text_events.last(),
            Some(&expected_result),
            "the real Host terminal text must match the entire Native result, including response.id: {case}"
        );

        let rows = JsonlReader::new(transcript_path, fs)
            .read_all()
            .await
            .expect("read actual runtime session JSONL");
        let assistant_rows = rows
            .iter()
            .filter(|row| row.message_type == "assistant")
            .collect::<Vec<_>>();
        assert!(
            assistant_rows.iter().any(|row| {
                row.message.string_units("/stop_details/explanation")
                    == Some(explanation_units.clone())
            }),
            "the production transcript row must retain Native explanation source units: {case}"
        );
        if category.is_empty() {
            assert!(
                assistant_rows.iter().any(|row| {
                    row.message
                        .at_pointer("/stop_details")
                        .and_then(|details| details.get_property("category"))
                        .is_none()
                }),
                "Native category is absent in the generic case: {case}"
            );
        } else {
            assert!(
                assistant_rows.iter().any(|row| {
                    row.message.string_at("/stop_details/category").as_deref() == Some(category)
                }),
                "Native refusal category must survive persistence: {case}"
            );
        }
    }
}
