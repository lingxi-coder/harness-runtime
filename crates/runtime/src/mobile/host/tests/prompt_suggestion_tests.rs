use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client::protocol::events::ClientEvent;
use lingxi_core::host::{OrchestratorHandle, PromptSuggestionContext};

use super::{build_mobile, test_config, FakeListener, HostFakePlatform, RecordingPermissionSink};

#[derive(Default)]
struct SuggestionAnalytics {
    events: tokio::sync::Mutex<Vec<(String, telemetry::LogEventMetadata)>>,
}

#[async_trait::async_trait]
impl telemetry::AnalyticsSink for SuggestionAnalytics {
    async fn log_event(&self, name: &str, metadata: telemetry::LogEventMetadata) {
        if name.contains("suggestion") || name == "tengu_feature_bad" {
            self.events.lock().await.push((name.into(), metadata));
        }
    }

    async fn log_event_async(&self, name: &str, metadata: telemetry::LogEventMetadata) {
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "mobile-suggestion-test"
    }
}

fn read_request(socket: &mut std::net::TcpStream) -> serde_json::Value {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0, "provider request ended before its headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .expect("JSON provider request has a content length");
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0, "provider request ended before its body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn respond(socket: &mut std::net::TcpStream, index: usize, text: &str) {
    let events = [
        serde_json::json!({"type":"message_start","message":{"id":format!("mobile-reply-{index}"),"type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ];
    let body = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

#[tokio::test]
async fn mobile_default_composer_generates_and_delivers_a_post_turn_suggestion() {
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let base = format!("http://{}", server.local_addr().unwrap());

    let root = tempfile::tempdir().unwrap();
    let mut config = test_config(root.path());
    config.workspace_trusted = true;
    config.api_base = base;
    config.api_key = "test-fixture-key".into();
    config.default_model = "claude-sonnet-4-5".into();
    let listener = Arc::new(FakeListener::default());
    let build_started = Instant::now();
    let runtime = build_mobile(
        config,
        Arc::new(HostFakePlatform::new(root.path().into())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
    )
    .await
    .unwrap();
    let build_elapsed = build_started.elapsed();
    let analytics = Arc::new(SuggestionAnalytics::default());
    runtime
        .mcp_tool_context
        .bus
        .attach_sink(analytics.clone())
        .await;
    let session_id = runtime
        .orchestrator
        .current_session_id()
        .await
        .as_uuid()
        .to_string();
    runtime
        .orchestrator
        .update_prompt_suggestion_context(PromptSuggestionContext {
            session_id: session_id.clone(),
            focus: "focused".into(),
            input_empty: true,
            pending_permission: false,
            elicitation_active: false,
        })
        .await
        .unwrap();

    // Mobile startup may load bundled resources before any model query. Start
    // the provider deadline only once the runtime and attached UI are ready.
    let (requests_tx, requests_rx) = tokio::sync::oneshot::channel();
    let server_thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (index, text) in [
            "I found the bug.",
            "The fix is ready.",
            "Run the focused tests",
        ]
        .into_iter()
        .enumerate()
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match server.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mobile provider request {index} never arrived: {error}"),
                }
            };
            requests.push(read_request(&mut socket));
            respond(&mut socket, index, text);
        }
        let _ = requests_tx.send(requests);
    });

    for prompt in ["Find the bug", "Fix it and then run the focused tests"] {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            runtime.orchestrator.run_turn_streaming(prompt),
        )
        .await;
        match result {
            Ok(outcome) => { outcome.unwrap(); }
            Err(error) => panic!(
                "foreground mobile turn timed out: {error}; build={build_elapsed:?}, session={session_id}, prompt={prompt:?}, events={:?}",
                listener.received.lock().await
            ),
        }
    }
    let suggestion_result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let events = listener.received.lock().await;
            if let Some(ClientEvent::PromptSuggestion {
                suggestion,
                uuid,
                session_id: owner,
            }) = events
                .iter()
                .find(|event| matches!(event, ClientEvent::PromptSuggestion { .. }))
            {
                assert_eq!(suggestion.display(), "Run the focused tests");
                assert_eq!(owner, &session_id);
                assert!(uuid::Uuid::parse_str(uuid).is_ok());
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, ClientEvent::TurnEnded { .. }))
                        .count(),
                    2
                );
                break;
            }
            drop(events);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if let Err(error) = suggestion_result {
        panic!(
            "default mobile composition did not publish a suggestion: {error}; session={session_id}; current_session={}; permission={:?}; analytics={:?}; events={:?}; history={:?}",
            runtime.orchestrator.current_session_id().await.as_uuid(),
            runtime.orchestrator.permission_mode(),
            analytics.events.lock().await,
            listener.received.lock().await,
            runtime.orchestrator.snapshot_history().await,
        );
    }
    let requests = requests_rx.await.unwrap();
    server_thread.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2]["messages"]
        .to_string()
        .contains("[SUGGESTION MODE:"));
    assert_eq!(requests[2]["stream"], true);
    assert_eq!(requests[2]["model"], requests[1]["model"]);
}
