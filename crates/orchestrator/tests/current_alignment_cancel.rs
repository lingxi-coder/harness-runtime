//! Replay Native 2.1.295's in-flight CLI SIGINT boundary through the public
//! Orchestrator entrypoint, real ProviderApiAdapter/ApiService/HttpTransport,
//! localhost HTTP, and JSONL writer. Native is never launched by this test.

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
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use tempfile::tempdir;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-sonnet-4-5";
const PROMPT: &str = "Reply with the fixed cancellation probe result.";
const INTERRUPT: &str = "[Request interrupted by user]";
const SESSION_ID: &str = "00000000-0000-4000-8000-000000002940";
const NATIVE_PROMPT_CONTENT: &[u8] = include_bytes!(
    "../../../docs/parity/current-alignment/fixtures/cli-cancel-295/native-baseline/2.1.295/prompt-content.raw.json"
);
const NATIVE_INTERRUPT_CONTENT: &[u8] = include_bytes!(
    "../../../docs/parity/current-alignment/fixtures/cli-cancel-295/native-baseline/2.1.295/interrupt-content.raw.json"
);

struct PendingHttpServer {
    address: SocketAddr,
    request: Option<oneshot::Receiver<Result<Vec<u8>, String>>>,
    disconnected: Option<oneshot::Receiver<Result<bool, String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PendingHttpServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("loopback fixture listener is nonblocking");
        let address = listener.local_addr().expect("loopback address");
        let (request_tx, request) = oneshot::channel();
        let (disconnected_tx, disconnected) = oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let accepted = loop {
                match listener.accept() {
                    Ok((stream, _)) => break Ok(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if worker_stop.load(Ordering::Acquire) {
                            break Err("loopback fixture stopped before accept".to_owned());
                        }
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => break Err(error.to_string()),
                }
            };
            let result = match accepted {
                Ok(mut stream) => match stream
                    .set_nonblocking(false)
                    .and_then(|()| stream.set_read_timeout(Some(std::time::Duration::from_secs(5))))
                    .map_err(|error| error.to_string())
                    .and_then(|()| {
                        read_http_request(&mut stream).map_err(|error| error.to_string())
                    }) {
                    Ok(request) => {
                        let _ = request_tx.send(Ok(request));
                        // Keep the socket open without sending response headers.
                        // Dropping the pending reqwest send future must close it.
                        let mut byte = [0_u8; 1];
                        match stream.read(&mut byte) {
                            Ok(0) => Ok(true),
                            Ok(_) => Ok(false),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    Err(error) => {
                        let _ = request_tx.send(Err(error.clone()));
                        Err(error)
                    }
                },
                Err(error) => {
                    let _ = request_tx.send(Err(error.clone()));
                    Err(error)
                }
            };
            let _ = disconnected_tx.send(result);
        });
        Self {
            address,
            request: Some(request),
            disconnected: Some(disconnected),
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for PendingHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed before completing HTTP request",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let expected = header_end + 4 + content_length;
        if request.len() >= expected {
            request.truncate(expected);
            return Ok(request);
        }
    }
}

fn api_service(base_url: &str, transport: Arc<dyn Transport>) -> Arc<ApiService> {
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
    let mut server = PendingHttpServer::start();
    let base_url = format!("http://{}", server.address);
    let transport: Arc<dyn Transport> = Arc::new(
        lingxi_llm_client::HttpTransport::with_client_configurator(|builder| builder.no_proxy())
            .expect("construct loopback-only HTTP transport"),
    );
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(&base_url, transport)));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::into_shared(
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
        orchestrator::native_user_message_source::with_sdk_source(
            turn_orchestrator.run_turn_streaming_with_cancel(PROMPT, turn_cancel),
        )
        .await
    });

    let raw_request = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.request.take().expect("request receiver is present"),
    )
    .await
    .expect("one provider HTTP request must arrive within five seconds")
    .expect("HTTP fixture worker must report the request")
    .expect("HTTP fixture must read the complete request");
    let header_end = raw_request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("request headers terminate");
    let request_line = String::from_utf8_lossy(&raw_request[..header_end]);
    assert!(
        request_line.starts_with("POST /v1/messages?beta=true "),
        "{request_line}"
    );
    let request_body: Value = serde_json::from_slice(&raw_request[header_end + 4..])
        .expect("streaming request body is JSON");
    assert_eq!(request_body["model"], MODEL);
    assert!(request_body["stream"].as_bool().unwrap_or(false));
    assert!(request_body["messages"].to_string().contains(PROMPT));

    // The server has received a complete request but is withholding response
    // headers. Cancellation must drop the request-open future itself.
    cancel.cancel();

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("cancelled turn must finish")
        .expect("turn task must not panic")
        .expect("orchestrator cancellation is a completed turn outcome");
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server
            .disconnected
            .take()
            .expect("disconnect receiver is present"),
    )
    .await
    .expect("cancel must close the pending HTTP connection")
    .expect("HTTP fixture worker must report socket closure")
    .expect("HTTP fixture socket read succeeds"));

    let events = output.snapshot().await;
    let end_reasons = events
        .iter()
        .filter_map(|event| match event {
            OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(end_reasons, vec!["aborted_streaming"]);

    let rows = JsonlReader::new(transcript_path.clone(), fs)
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

    // Native 2.1.295 stores the initial plain SDK prompt as a JSON string, but
    // writes its injected interrupt as a text-block array. Compare those exact
    // source slices from the real JSONL bytes, not normalized parsed values.
    let transcript_raw = std::fs::read(&transcript_path).expect("read raw JSONL bytes");
    let raw_rows = transcript_raw
        .split(|byte| *byte == b'\n')
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>();
    let prompt_slice_matches = raw_rows
        .iter()
        .filter(|row| {
            row.windows(NATIVE_PROMPT_CONTENT.len())
                .any(|w| w == NATIVE_PROMPT_CONTENT)
        })
        .count();
    let interrupt_slice_matches = raw_rows
        .iter()
        .filter(|row| {
            row.windows(NATIVE_INTERRUPT_CONTENT.len())
                .any(|w| w == NATIVE_INTERRUPT_CONTENT)
        })
        .count();
    assert_eq!(
        prompt_slice_matches, 1,
        "persist the Native prompt JSON slice byte-for-byte"
    );
    assert_eq!(
        interrupt_slice_matches, 1,
        "persist the Native interruption JSON slice byte-for-byte"
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
