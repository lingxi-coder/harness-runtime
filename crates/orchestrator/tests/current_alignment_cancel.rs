//! Replay Native 2.1.293's in-flight CLI SIGINT boundary through the public
//! Orchestrator entrypoint, real ProviderApiAdapter/ApiService, and JSONL writer.
//! Native's process-level SIGINT wiring itself remains a separate Product gate.

use async_trait::async_trait;
use lingxi_core::host::{FileSystem, OutputEvent};
use lingxi_core::types::SessionId;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter, TurnOutcome};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter, JsonlMessage};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tempfile::tempdir;
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-sonnet-4-5";
const PROMPT: &str = "Reply with the fixed cancellation probe result.";
const INTERRUPT: &str = "[Request interrupted by user]";
const SESSION_ID: &str = "00000000-0000-4000-8000-000000002940";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestShape {
    model: Option<String>,
    stream: Option<bool>,
}

fn request_shape(request: &lingxi_llm_client::HttpRequest) -> RequestShape {
    let body: Option<Value> = serde_json::from_slice(request.body.as_ref()).ok();
    RequestShape {
        model: body
            .as_ref()
            .and_then(|body| body.get("model"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        stream: body
            .as_ref()
            .and_then(|body| body.get("stream"))
            .and_then(Value::as_bool),
    }
}

struct WaitForCancelTransport {
    calls: AtomicUsize,
    request_shapes: Mutex<Vec<RequestShape>>,
    opened: Arc<Notify>,
    release: watch::Receiver<bool>,
}

#[async_trait]
impl Transport for WaitForCancelTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.request_shapes
            .lock()
            .expect("request-shape mutex")
            .push(request_shape(&request));
        self.opened.notify_one();

        // Keep the physical provider request pending until the test cancels the
        // public turn and releases this deterministic transport gate.
        let mut release = self.release.clone();
        while !*release.borrow() {
            if release.changed().await.is_err() {
                break;
            }
        }

        // Native's 2.1.293 fixture receives no response body before SIGINT.
        // Returning a successful empty SSE body models the closed response
        // boundary without inventing a provider error payload.
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Vec::<u8>::new().into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<WaitForCancelTransport>) -> Arc<ApiService> {
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
            "current-alignment-cancel",
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

fn fixed_session_id() -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(SESSION_ID).expect("fixed session id"))
}

fn user_text_units(row: &JsonlMessage) -> Option<Vec<u16>> {
    if row.message.string_units("/content").is_some() {
        return row.message.string_units("/content");
    }
    row.message.string_units("/content/0/text")
}

#[tokio::test]
async fn native_inflight_sigint_contract_reaches_orchestrator_output_and_jsonl() {
    let dir = tempdir().expect("isolated transcript directory");
    let transcript_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));
    let opened = Arc::new(Notify::new());
    let (release_tx, release_rx) = watch::channel(false);
    let transport = Arc::new(WaitForCancelTransport {
        calls: AtomicUsize::new(0),
        request_shapes: Mutex::new(Vec::new()),
        opened: opened.clone(),
        release: release_rx,
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = Arc::new(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 1,
                system_prompt_override: Some("Fixed in-flight cancellation fixture.".into()),
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

    let cancel = CancellationToken::new();
    let turn_orchestrator = orchestrator.clone();
    let turn_cancel = cancel.clone();
    let turn = tokio::spawn(async move {
        turn_orchestrator
            .run_turn_streaming_with_cancel(PROMPT, turn_cancel)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), opened.notified())
        .await
        .expect("one provider request must enter the fixed waiting transport");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    cancel.cancel();
    release_tx
        .send(true)
        .expect("release waiting provider response");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("cancelled turn must finish")
        .expect("turn task must not panic")
        .expect("orchestrator cancellation is a completed turn outcome");
    assert_eq!(outcome, TurnOutcome::Cancelled);

    let shapes = transport
        .request_shapes
        .lock()
        .expect("request-shape mutex")
        .clone();
    assert_eq!(
        shapes,
        vec![RequestShape {
            model: Some(MODEL.to_owned()),
            stream: Some(true),
        }],
        "the fixed transport receives one streaming request and no retry"
    );

    let events = output.snapshot().await;
    let end_reasons = events
        .iter()
        .filter_map(|event| match event {
            OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(end_reasons, vec!["aborted_streaming"]);

    let rows = JsonlReader::new(transcript_path, fs)
        .read_all()
        .await
        .expect("read actual Orchestrator JSONL output");
    let user_rows = rows
        .iter()
        .filter(|row| row.message_type == "user")
        .map(|row| user_text_units(row).expect("Native-compatible user text row"))
        .collect::<Vec<_>>();
    assert_eq!(
        user_rows,
        vec![
            PROMPT.encode_utf16().collect::<Vec<_>>(),
            INTERRUPT.encode_utf16().collect()
        ],
        "persist exact prompt then the Native interruption row"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.message_type == "assistant")
            .count(),
        0,
        "an interrupted request with no response does not persist an assistant row"
    );

    // Native creates message UUIDs and timestamps dynamically. Check their
    // format and parent references without treating the full JSONL as a byte
    // baseline; the observable prompt/interrupt text and order stay exact.
    let mut prior = std::collections::HashSet::<String>::new();
    for row in &rows {
        assert_eq!(row.session_id, SESSION_ID);
        assert!(session::jsonl::uuid::validate_uuid(&row.uuid));
        if let Some(parent) = &row.parent_uuid {
            assert!(
                prior.contains(parent),
                "parent UUID must point to an earlier row"
            );
        }
        prior.insert(row.uuid.clone());
    }
}
