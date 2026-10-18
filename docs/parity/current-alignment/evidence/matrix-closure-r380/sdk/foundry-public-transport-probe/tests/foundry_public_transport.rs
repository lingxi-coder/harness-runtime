use lingxi_llm_client::protocol::{ChatRequest, ProviderProfile, Secret, StreamEvent};
use lingxi_llm_client::{HttpTransport, LlmClientBuilder, RequestOptions};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const DUMMY_KEY: &str = "r380-localhost-only-dummy-key";
const ASSISTANT_TEXT: &str = "foundry-loopback-ok";
const IO_TIMEOUT: Duration = Duration::from_secs(5);

const SSE_FRAMES: [(&str, &str); 6] = [
    (
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_r380_foundry","type":"message","role":"assistant","model":"custom-deployment","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":0}}}"#,
    ),
    (
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    ),
    (
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"foundry-loopback-ok"}}"#,
    ),
    (
        "content_block_stop",
        r#"{"type":"content_block_stop","index":0}"#,
    ),
    (
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
    ),
    ("message_stop", r#"{"type":"message_stop"}"#),
];

fn foundry_profile(base_url: &str) -> ProviderProfile {
    serde_json::from_value(json!({
        "provider_id": "r380-local-foundry",
        "profile_name": "r380-local-foundry",
        "protocol": "foundry_claude",
        "base_url": base_url,
        "auth": "api_key",
        "regions": ["international"],
        "models": [{
            "display_model": "foundry-local-model",
            "request_model": "custom-deployment",
            "billing_model": "foundry-test-billing-model",
            "foundry": {
                "hosting": "anthropic",
                "model_id": "claude-opus-5-5"
            }
        }],
        "extra": {}
    }))
    .expect("the local Foundry profile fixture parses")
}

fn chat_request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "custom-deployment",
        "max_tokens": 16,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "Return the local Foundry fixture response."}]
        }]
    }))
    .expect("the chat request fixture parses")
}

fn sse_body() -> Vec<u8> {
    SSE_FRAMES
        .iter()
        .map(|(event, payload)| format!("event: {event}\r\ndata: {payload}\r\n\r\n"))
        .collect::<String>()
        .into_bytes()
}

fn raw_http_response() -> Vec<u8> {
    let body = sse_body();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

async fn read_one_request(socket: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 1024];
        let read = socket.read(&mut chunk).await.expect("read local request");
        assert_ne!(read, 0, "request ended before its declared body arrived");
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_text =
            std::str::from_utf8(&bytes[..header_end]).expect("HTTP headers are ASCII");
        let content_length = header_text
            .lines()
            .skip(1)
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .expect("the built-in transport declares its request body length");
        let request_end = header_end + 4 + content_length;
        if bytes.len() >= request_end {
            bytes.truncate(request_end);
            return bytes;
        }
    }
}

async fn serve_one_local_request(listener: TcpListener, response: Vec<u8>) -> (Vec<u8>, bool) {
    let (mut socket, peer) = timeout(IO_TIMEOUT, listener.accept())
        .await
        .expect("SDK reached the loopback listener before timeout")
        .expect("accept local request");
    assert!(peer.ip().is_loopback(), "the probe accepts localhost only");
    let request = timeout(IO_TIMEOUT, read_one_request(&mut socket))
        .await
        .expect("SDK request body arrived before timeout");
    socket
        .write_all(&response)
        .await
        .expect("write fixed local SSE response");
    socket.shutdown().await.expect("close local HTTP response");

    let second_request = timeout(Duration::from_millis(100), listener.accept())
        .await
        .is_ok();
    (request, second_request)
}

fn split_raw_request(raw: &[u8]) -> (String, Vec<String>, Vec<u8>) {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("request has an HTTP header terminator");
    let head = std::str::from_utf8(&raw[..header_end]).expect("request headers are ASCII");
    let mut lines = head.split("\r\n");
    let request_line = lines.next().expect("HTTP request line").to_owned();
    let headers = lines.map(str::to_owned).collect();
    let body = raw[header_end + 4..].to_vec();
    (request_line, headers, body)
}

fn header_value<'a>(headers: &'a [String], wanted: &str) -> Option<&'a str> {
    headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(wanted).then_some(value.trim())
    })
}

fn header_count(headers: &[String], wanted: &str) -> usize {
    headers
        .iter()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(wanted))
        })
        .count()
}

fn sse_event_names(body: &[u8]) -> Vec<String> {
    std::str::from_utf8(body)
        .expect("response SSE body is UTF-8")
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("event: "))
        .map(str::to_owned)
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("write into String");
    }
    output
}

#[tokio::test]
async fn foundry_public_sdk_sends_and_consumes_one_localhost_sse_exchange() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local-only stub");
    let address = listener.local_addr().expect("local listener address");
    assert!(address.ip().is_loopback());
    let base_url = format!("http://{address}/anthropic");
    assert!(base_url.starts_with("http://127.0.0.1:"));
    let response = raw_http_response();
    let response_for_receipt = response.clone();
    let server = tokio::spawn(serve_one_local_request(listener, response));

    // Use the production SDK client and production HttpTransport. `no_proxy`
    // prevents inherited machine proxy configuration from routing this local
    // request off-host.
    let transport = std::sync::Arc::new(
        HttpTransport::with_client_configurator(|builder| builder.no_proxy())
            .expect("build production transport with proxy bypass"),
    );
    let profile = foundry_profile(&base_url);
    let client = LlmClientBuilder::with_transport(transport, &[profile])
        .with_region(lingxi_llm_client::protocol::Region::International)
        .build()
        .expect("build current public SDK client");
    let options = RequestOptions {
        credential: Some(Secret::new(DUMMY_KEY.to_owned())),
        ..Default::default()
    };
    let request = chat_request();
    let mut stream = client
        .chat()
        .stream(&request, &options)
        .await
        .expect("public Foundry route reaches the local HTTP stub");

    let mut public_events = Vec::new();
    let mut normalized_events = Vec::new();
    let mut raw_provider_events = Vec::new();
    let mut text = String::new();
    let mut output_model = None;
    let mut final_usage = None;
    let mut final_stop = None;
    while let Some(event) = stream.next().await {
        match event.expect("current public SDK consumes each response frame") {
            StreamEvent::ProviderEvent { protocol, payload } => {
                assert_eq!(
                    protocol,
                    lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages
                );
                let event_type = payload
                    .string_at("/type")
                    .expect("provider event preserves its source type");
                public_events.push(format!("provider:{event_type}"));
                raw_provider_events.push((event_type, payload.as_json_source().to_owned()));
            }
            StreamEvent::Start { model, .. } => {
                public_events.push("start".to_owned());
                normalized_events.push("start".to_owned());
                output_model = Some(model);
            }
            StreamEvent::TextDelta { text: delta, .. } => {
                public_events.push("text_delta".to_owned());
                normalized_events.push("text_delta".to_owned());
                text.push_str(&delta);
            }
            StreamEvent::TextDeltaJsUtf16 {
                text: delta,
                utf16_code_units,
                ..
            } => {
                public_events.push("text_delta".to_owned());
                normalized_events.push("text_delta".to_owned());
                assert_eq!(String::from_utf16_lossy(&utf16_code_units), delta);
                text.push_str(&delta);
            }
            StreamEvent::BlockEnd { .. } => {
                public_events.push("block_end".to_owned());
                normalized_events.push("block_end".to_owned());
            }
            StreamEvent::End {
                stop_reason, usage, ..
            } => {
                public_events.push("end".to_owned());
                normalized_events.push("end".to_owned());
                final_stop = Some(format!("{stop_reason:?}"));
                final_usage = Some(usage);
            }
            StreamEvent::Inference { .. } => public_events.push("inference".to_owned()),
            other => panic!("unexpected current public SDK event: {other:?}"),
        }
    }

    let (raw_request, second_request) = server.await.expect("local server task completed");
    assert!(!second_request, "the SDK sends exactly one request");
    let (request_line, ordered_headers, request_body) = split_raw_request(&raw_request);
    assert_eq!(request_line, "POST /anthropic/v1/messages HTTP/1.1");
    let host = address.to_string();
    assert_eq!(header_value(&ordered_headers, "host"), Some(host.as_str()));
    assert_eq!(header_count(&ordered_headers, "x-api-key"), 1);
    assert_eq!(header_value(&ordered_headers, "x-api-key"), Some(DUMMY_KEY));
    assert_eq!(header_count(&ordered_headers, "authorization"), 0);
    assert_eq!(
        header_value(&ordered_headers, "content-type"),
        Some("application/json")
    );
    assert_eq!(
        header_value(&ordered_headers, "anthropic-version"),
        Some("2023-06-01")
    );
    assert_eq!(header_count(&ordered_headers, "transfer-encoding"), 0);
    let declared_length: usize = header_value(&ordered_headers, "content-length")
        .expect("the request has a content length")
        .parse()
        .expect("the content length is numeric");
    assert_eq!(declared_length, request_body.len());
    let request_json: Value =
        serde_json::from_slice(&request_body).expect("physical request body is JSON");
    assert_eq!(request_json["model"], "custom-deployment");
    assert_eq!(request_json["stream"], true);
    assert_eq!(request_json["max_tokens"], 16);
    assert_eq!(
        request_json["messages"][0]["content"][0]["text"],
        "Return the local Foundry fixture response."
    );
    assert!(request_json.get("foundry").is_none());
    assert!(request_json.get("model_id").is_none());

    let response_header_end = response_for_receipt
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("fixed raw response has an HTTP header terminator");
    let response_body = &response_for_receipt[response_header_end + 4..];
    let raw_events = sse_event_names(response_body);
    assert_eq!(
        raw_events,
        SSE_FRAMES
            .iter()
            .map(|(event, _)| (*event).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(text, ASSISTANT_TEXT);
    assert_eq!(output_model.as_deref(), Some("custom-deployment"));
    assert_eq!(
        normalized_events,
        vec![
            "start".to_owned(),
            "text_delta".to_owned(),
            "block_end".to_owned(),
            "end".to_owned(),
        ]
    );
    assert_eq!(
        raw_provider_events
            .iter()
            .map(|(event, _)| event.as_str())
            .collect::<Vec<_>>(),
        vec!["message_delta"]
    );
    let expected_provider_events = SSE_FRAMES
        .iter()
        .filter(|(event, _)| *event == "message_delta")
        .map(|(_, payload)| payload.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        raw_provider_events
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect::<Vec<_>>(),
        expected_provider_events,
        "public SDK preserves raw Foundry stream observations in source order"
    );
    let usage = final_usage.expect("the public SDK emitted a terminal usage report");
    let usage_value = usage.usage.expect("the fixture reports final usage");
    assert_eq!(usage_value.input_tokens, 7);
    assert_eq!(usage_value.output_tokens, 4);
    assert_eq!(final_stop.as_deref(), Some("EndTurn"));

    let output = json!({
        "probe": "r380 current public SDK Foundry route over production HttpTransport and localhost only",
        "external_provider_network_used": false,
        "native_process_or_capture_used": false,
        "transport": "HttpTransport::with_client_configurator(no_proxy)",
        "request_count": 1,
        "selected_base_url": base_url,
        "auth_input": {
            "strategy": "api_key",
            "source": "RequestOptions.credential",
            "header": "x-api-key",
            "fixture_credential": DUMMY_KEY,
            "real_secret_used": false
        },
        "request": {
            "raw_http_hex": hex(&raw_request),
            "request_line": request_line,
            "ordered_header_lines": ordered_headers,
            "body_bytes": request_body.len(),
            "body_hex": hex(&request_body),
            "body_utf8": String::from_utf8(request_body).expect("JSON body is UTF-8")
        },
        "response": {
            "raw_http_hex": hex(&response_for_receipt),
            "raw_http_utf8": String::from_utf8(response_for_receipt).expect("fixed response is UTF-8"),
            "sse_event_sequence": raw_events
        },
        "public_sdk_consumption": {
            "event_sequence": public_events,
            "raw_provider_event_sequence": raw_provider_events.iter().map(|(event, _)| event).collect::<Vec<_>>(),
            "assistant_text": text,
            "model": output_model,
            "stop_reason": final_stop,
            "input_tokens": usage_value.input_tokens,
            "output_tokens": usage_value.output_tokens
        }
    });
    println!("R380_FOUNDRY_LOCAL_TRANSPORT={output}");
}
