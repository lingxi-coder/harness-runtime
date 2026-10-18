//! Native 2.1.293 HTTP/1 layout replay through ApiService and the built-in
//! `HttpTransport`. The loopback listener captures the bytes actually written
//! by reqwest; no request body or header block is assembled by the test.
use futures::StreamExt;
use lingxi_core::types::{ConversationMessage, MessageId, SessionId};
use lingxi_llm_client::providers::anthropic::{
    request_policy::AnthropicClientMetadata,
    system_prompt::{PromptText, SystemPromptInput},
};
use llm_runtime::history::{HistoryContentDelta, HistoryEvent};
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    ApiService, ClientConfig, LlmRequest, ModelRuntime, ProtocolFamily, ProviderId,
    RequestMetadata, SubscriberState,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};

const MODEL: &str = "claude-sonnet-5-5";
const SESSION_ID: &str = "6b7ff52d-e492-4084-88e2-5ad153e3c053";
const USER_TEXT: &str = "Return HEADLESS_LOCAL_RESPONSE.";
const VERSION_293: &str = "2.1.293";
const VERSION_295: &str = "2.1.295";
const BILLING_SYSTEM_293: &str =
    "x-anthropic-billing-header: cc_version=2.1.293.7c5; cc_entrypoint=sdk-cli;";
const BILLING_SYSTEM_295: &str =
    "x-anthropic-billing-header: cc_version=2.1.295.fa6; cc_entrypoint=sdk-cli;";
const SDK_SYSTEM: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const CUSTOM_SYSTEM: &str = "You are a local deterministic headless fixture.";
const KEY_ENV: &str = "LINGXI_M2_HTTP_LAYOUT_DUMMY_KEY";
const NATIVE_ASSUME_FIRST_PARTY_ENV: &str = "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL";
const BARE_POLICY_ENV: &str = branding::SIMPLE_ENV;
const DUMMY_KEY: &str = "sk-ant-headless-local-fixture-only";
static HTTP_LAYOUT_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvRestore {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvRestore {
    fn set(name: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(name);
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
        Self { name, previous }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

struct CapturedRequest {
    request_line: String,
    headers: Vec<(String, String)>,
    header_bytes: Vec<u8>,
    body: Vec<u8>,
    local_address: SocketAddr,
    used_native_port: bool,
}

async fn read_request(
    stream: &mut TcpStream,
    local_address: SocketAddr,
    used_native_port: bool,
) -> CapturedRequest {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .expect("loopback request read timed out")
            .expect("loopback request read failed");
        assert_ne!(count, 0, "loopback peer closed before sending request");
        received.extend_from_slice(&buffer[..count]);
        let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = std::str::from_utf8(&received[..end]).expect("HTTP header block is ASCII");
        let length = head
            .split("\r\n")
            .skip(1)
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid Content-Length"))
            })
            .expect("NativeFetch must frame the request with Content-Length");
        if received.len() >= end + 4 + length {
            break (end, length);
        }
    };

    let header_bytes = received[..header_end + 4].to_vec();
    let head = std::str::from_utf8(&received[..header_end]).expect("HTTP header block is ASCII");
    let mut lines = head.split("\r\n");
    let request_line = lines.next().expect("request line").to_owned();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').expect("header field separator");
            (name.to_owned(), value.trim_start().to_owned())
        })
        .collect();
    let body_start = header_end + 4;
    let body = received[body_start..body_start + content_length].to_vec();
    CapturedRequest {
        request_line,
        headers,
        header_bytes,
        body,
        local_address,
        used_native_port,
    }
}

async fn loopback_server(
    preferred_port: Option<u16>,
    offered_response: Vec<u8>,
) -> (SocketAddr, JoinHandle<CapturedRequest>) {
    let (listener, used_native_port) = if let Some(port) = preferred_port {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => (listener, true),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => (
                TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind dynamic loopback HTTP fixture"),
                false,
            ),
            Err(error) => panic!("bind preferred Native fixture port {port}: {error}"),
        }
    } else {
        (
            TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback HTTP fixture"),
            false,
        )
    };
    let address = listener.local_addr().expect("loopback address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept fixture request");
        let request = read_request(&mut stream, address, used_native_port).await;
        // The gate concerns the outbound HTTP request. A completed empty SSE
        // response is enough to let the real transport return its stream.
        stream
            .write_all(&offered_response)
            .await
            .expect("write loopback response");
        request
    });
    (address, server)
}

fn runtime(base_url: &str, protocol: ProtocolFamily, provider_id: ProviderId) -> ModelRuntime {
    let protocol = serde_json::to_value(protocol).expect("protocol serialization");
    let provider_id = serde_json::to_value(provider_id).expect("provider serialization");
    let config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": provider_id,
            "profile_name": "direct",
            "base_url": base_url,
            "protocol": protocol,
            "auth": "api_key",
            "credential": {"type": "env", "var": KEY_ENV},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": true,
                    "structured_output": false
                }
            }]
        }]
    }))
    .expect("test client config");
    ModelRuntime::from_config(config).expect("model runtime")
}

fn anthropic_system(billing_system: &str) -> SystemPromptInput {
    // These three exact strings are explicit test inputs from the Native
    // print-text capture. This replays the ApiService seam; it does not claim
    // that the full LingXi CLI assembled Native's billing or SDK prefixes.
    SystemPromptInput::source_vector(
        vec![
            PromptText::from_string(billing_system),
            PromptText::from_string(SDK_SYSTEM),
            PromptText::from_string(CUSTOM_SYSTEM),
        ],
        None,
        None,
    )
}

fn service(
    runtime: ModelRuntime,
    metadata_headers: bool,
    request_metadata: bool,
    native_endpoint: Option<&str>,
    native_version: &str,
) -> ApiService {
    let user_agent = UserAgentEnv {
        user_type: Some("external".into()),
        entrypoint: Some("sdk-cli".into()),
        ..UserAgentEnv::default()
    };
    let service = ApiService::new_with_routing(
        Arc::new(runtime),
        Arc::new(
            lingxi_llm_client::HttpTransport::with_client_configurator(|builder| {
                builder.no_proxy()
            })
            .expect("built-in HTTP transport"),
        ),
        SubscriberState::default(),
        user_agent,
        native_version,
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    );
    let service = if native_endpoint.is_some() {
        service.with_interactive_session(false)
    } else {
        service
    };
    let service = if request_metadata {
        let user_id = format!(
            r#"{{"device_id":"{}","account_uuid":"","session_id":"{}"}}"#,
            "0".repeat(64),
            SESSION_ID
        );
        service.with_request_metadata(RequestMetadata { user_id })
    } else {
        service
    };
    if metadata_headers {
        // Keep session_id unset on the static metadata. The Host must derive
        // the header from the current request's Core task-local origin.
        let metadata = AnthropicClientMetadata::claude_code_2_1_293("darwin", "arm64");
        assert!(metadata.session_id.is_none());
        let service = service.with_anthropic_client_metadata(metadata);
        if let Some(native_endpoint) = native_endpoint {
            service.with_native_anthropic_endpoint(native_endpoint)
        } else {
            service
        }
    } else {
        service
    }
}

async fn capture_anthropic(
    metadata_headers: bool,
    request_metadata: bool,
    native_endpoint: bool,
    native_version: &str,
    billing_system: &str,
    native_response: &[u8],
    preferred_port: Option<u16>,
) -> CapturedRequest {
    let (address, server) = loopback_server(preferred_port, native_response.to_vec()).await;
    // The checked-in Native capture ran without the process-level assume gate.
    // Host NativeFetch admission below is a separate explicit endpoint choice.
    let _native_assume = EnvRestore::set(NATIVE_ASSUME_FIRST_PARTY_ENV, None);
    let _bare = EnvRestore::set(BARE_POLICY_ENV, Some("1"));
    let base_url = format!("http://{address}");
    // Native's TBC predicate reads ANTHROPIC_BASE_URL from the process
    // environment. The explicit service endpoint selects the loopback route
    // for HTTP, but does not replace that independent source input.
    let _native_base_url =
        native_endpoint.then(|| EnvRestore::set("ANTHROPIC_BASE_URL", Some(&base_url)));
    let service = service(
        runtime(
            &base_url,
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
        ),
        metadata_headers,
        request_metadata,
        native_endpoint.then_some(base_url.as_str()),
        native_version,
    );
    let system = anthropic_system(billing_system);
    let request_session_id =
        SessionId::from_uuid(uuid::Uuid::parse_str(SESSION_ID).expect("fixed Native session UUID"));
    // The same fixed session origin used by Native is visible only while the
    // public ApiService request is built and sent; no boot-session fallback is
    // involved in the expected X-Claude-Code-Session-Id header.
    let mut result = lingxi_core::host::session_flags::scope_request_session_id(
        request_session_id,
        service.stream_with_system_prompt(
            MODEL,
            None,
            Some(&system),
            vec![ConversationMessage::user(
                MessageId::new(),
                USER_TEXT.to_owned(),
            )],
            Vec::new(),
            None,
            Some(json!("medium")),
            None,
            false,
            Some("repl_main_thread"),
            None,
            Vec::new(),
            None,
            None,
            // This standalone fixture contains only its user turn; it has no
            // transcript-attachment rows with typed message identities.
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
            None,
        ),
    )
    .await
    .expect("ApiService request should reach loopback transport");
    let mut response_text = String::new();
    while let Some(event) = result.next().await {
        if let HistoryEvent::ContentBlockDelta { delta, .. } =
            event.expect("Native offered SSE response should decode")
        {
            match delta {
                HistoryContentDelta::TextDelta { text }
                | HistoryContentDelta::TextJsUtf16Delta { text, .. } => {
                    response_text.push_str(&text)
                }
                _ => {}
            }
        }
    }
    assert_eq!(response_text, "HEADLESS_LOCAL_RESPONSE");
    let captured = server.await.expect("loopback server task");
    let expected_host = address.to_string();
    assert_eq!(header(&captured, "Host"), Some(expected_host.as_str()));
    captured
}

async fn capture_openai_with_anthropic_metadata() -> CapturedRequest {
    let response_body = b"data: [DONE]\n\n";
    let mut offered_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        response_body.len()
    )
    .into_bytes();
    offered_response.extend_from_slice(response_body);
    let (address, server) = loopback_server(None, offered_response).await;
    let _native_assume = EnvRestore::set(NATIVE_ASSUME_FIRST_PARTY_ENV, None);
    let base_url = format!("http://{address}/v1");
    let service = service(
        runtime(
            &base_url,
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAICompatible {
                name: "fixture-openai".into(),
            },
        ),
        true,
        false,
        None,
        VERSION_293,
    );
    let result = service
        .stream_request(LlmRequest::new(MODEL).with_user_text(USER_TEXT))
        .await
        .expect("OpenAI Chat request should reach loopback transport");
    drop(result);
    let captured = server.await.expect("loopback server task");
    let expected_host = address.to_string();
    assert_eq!(header(&captured, "Host"), Some(expected_host.as_str()));
    captured
}

fn header<'a>(request: &'a CapturedRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn header_names(request: &CapturedRequest) -> Vec<String> {
    request
        .headers
        .iter()
        .map(|(name, _)| name.clone())
        .collect()
}

fn assert_absent_native_headers(request: &CapturedRequest, control: &str) {
    for name in [
        "X-Claude-Code-Session-Id",
        "X-Stainless-Arch",
        "X-Stainless-Lang",
        "X-Stainless-OS",
        "X-Stainless-Package-Version",
        "X-Stainless-Retry-Count",
        "X-Stainless-Runtime",
        "X-Stainless-Runtime-Version",
        "X-Stainless-Timeout",
        "anthropic-dangerous-direct-browser-access",
        "x-app",
    ] {
        assert!(
            header(request, name).is_none(),
            "{control}: unexpected {name}"
        );
    }
}

fn first_difference(expected: &[u8], actual: &[u8]) -> usize {
    expected
        .iter()
        .zip(actual)
        .position(|(left, right)| left != right)
        .unwrap_or(expected.len().min(actual.len()))
}

fn raw_difference(field: &str, expected: &[u8], actual: &[u8]) -> String {
    let offset = first_difference(expected, actual);
    let window = |bytes: &[u8]| {
        let start = offset.saturating_sub(12).min(bytes.len());
        let end = (offset + 20).min(bytes.len());
        bytes[start..end]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        "{field} differs at raw byte {offset}; expected_len={}, actual_len={}, expected_window_hex={}, actual_window_hex={}",
        expected.len(),
        actual.len(),
        window(expected),
        window(actual),
    )
}

fn save_failed_raw_capture(version: &str, request: &CapturedRequest) -> std::io::Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "current-alignment-http-layout-{version}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("request-line.raw"),
        request.request_line.as_bytes(),
    )?;
    fs::write(directory.join("request-headers.raw"), &request.header_bytes)?;
    fs::write(directory.join("request-body.raw"), &request.body)?;
    Ok(directory)
}

fn raw_header_mismatch(
    expected: &[u8],
    request: &CapturedRequest,
    native_port: u16,
) -> Option<String> {
    let actual = request.header_bytes.as_slice();
    if request.used_native_port {
        assert_eq!(request.local_address.port(), native_port);
        return (actual != expected)
            .then(|| raw_difference("fixed-port raw header block", expected, actual));
    }

    let expected_host_start = expected
        .windows(b"Host: 127.0.0.1:".len())
        .position(|window| window == b"Host: 127.0.0.1:")
        .expect("Native header fixture Host line");
    let actual_host_start = actual
        .windows(b"Host: 127.0.0.1:".len())
        .position(|window| window == b"Host: 127.0.0.1:")
        .expect("candidate loopback Host line");
    let host_line_end = |bytes: &[u8], start: usize| {
        start
            + bytes[start..]
                .windows(2)
                .position(|window| window == b"\r\n")
                .expect("Host line CRLF")
            + 2
    };
    let expected_host_end = host_line_end(expected, expected_host_start);
    let actual_host_end = host_line_end(actual, actual_host_start);
    let expected_host = &expected[expected_host_start..expected_host_end];
    let actual_host = &actual[actual_host_start..actual_host_end];
    let native_host = format!("Host: 127.0.0.1:{native_port}\r\n");
    assert_eq!(expected_host, native_host.as_bytes());
    let required_host = format!("Host: {}\r\n", request.local_address);
    if actual_host != required_host.as_bytes() {
        return Some(format!(
            "dynamic Host constraint failed: expected {:?}, actual {:?}",
            required_host.as_bytes(),
            actual_host
        ));
    }
    if actual[..actual_host_start] != expected[..expected_host_start] {
        return Some(raw_difference(
            "raw header prefix before Host",
            &expected[..expected_host_start],
            &actual[..actual_host_start],
        ));
    }
    if actual[actual_host_end..] != expected[expected_host_end..] {
        return Some(raw_difference(
            "raw header suffix after dynamic Host",
            &expected[expected_host_end..],
            &actual[actual_host_end..],
        ));
    }
    None
}

fn native_body_with_fixed_device_id(native_body: &[u8]) -> Vec<u8> {
    // The only permitted body difference in this fixture is the Native
    // metadata.user_id.device_id identity value. Keep the captured body raw;
    // replace its exact 64-byte range without parsing or reserializing JSON.
    const DEVICE_ID_OFFSET: usize = 628;
    const DEVICE_ID_LEN: usize = 64;
    let device_id = native_body
        .get(DEVICE_ID_OFFSET..DEVICE_ID_OFFSET + DEVICE_ID_LEN)
        .expect("Native device_id byte range");
    assert!(device_id
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)));
    let mut expected = native_body.to_vec();
    expected[DEVICE_ID_OFFSET..DEVICE_ID_OFFSET + DEVICE_ID_LEN].fill(b'0');
    expected
}

#[tokio::test]
async fn native_293_http_layout_is_selected_only_for_opted_in_first_party_anthropic() {
    let _serial = HTTP_LAYOUT_ENV_LOCK.lock().await;
    let _key = EnvRestore::set(KEY_ENV, Some(DUMMY_KEY));

    let native_response = include_bytes!("fixtures/http-layout-293/native-offered-response.raw");
    let native = capture_anthropic(
        true,
        true,
        true,
        VERSION_293,
        BILLING_SYSTEM_293,
        native_response,
        Some(54086),
    )
    .await;
    let receipt: Value = serde_json::from_str(include_str!(
        "../../../docs/parity/current-alignment/fixtures/http-layout/native-receipt.json"
    ))
    .expect("tracked Native receipt");
    let expected_line = receipt["http1Observation"]["requestLine"]
        .as_str()
        .expect("Native request line");
    let expected_headers: Vec<String> = receipt["http1Observation"]["headerNamesInObservedOrder"]
        .as_array()
        .expect("Native ordered header names")
        .iter()
        .map(|name| name.as_str().expect("header name").to_owned())
        .collect();

    let request_line_mismatch = (native.request_line != expected_line).then(|| {
        format!(
            "request line differs: expected {expected_line:?}, actual {:?}",
            native.request_line
        )
    });
    let header_names_mismatch = (header_names(&native) != expected_headers).then(|| {
        format!(
            "ordered header names differ: expected {expected_headers:?}, actual {:?}",
            header_names(&native)
        )
    });
    assert!(native.header_bytes.ends_with(b"\r\n\r\n"));
    assert_eq!(
        header(&native, "Host"),
        Some(native.local_address.to_string().as_str())
    );

    // The checked-in template is the Native 2.1.293 raw body with only the
    // existing nested metadata.user_id.device_id identity scalar replaced by
    // 64 zeroes. The candidate body itself is captured from ApiService and the
    // built-in HttpTransport; it is never hand-serialized here.
    let expected_body =
        include_bytes!("fixtures/http-layout-293/native-request-body-template.raw.json");
    let body_mismatch = if native.body.as_slice() == expected_body {
        None
    } else {
        Some(raw_difference("body", expected_body, &native.body))
    };

    // Control A: first-party route with no Native metadata or explicit endpoint
    // selection keeps the ordinary SDK request header policy.
    let no_metadata = capture_anthropic(
        false,
        true,
        false,
        VERSION_293,
        BILLING_SYSTEM_293,
        native_response,
        None,
    )
    .await;
    assert_eq!(no_metadata.request_line, expected_line);
    assert_absent_native_headers(&no_metadata, "first-party without metadata");

    // Control B: metadata alone is not route selection. A gateway without the
    // explicit Native endpoint keeps the ordinary HTTP layout and headers.
    let gateway = capture_anthropic(
        true,
        true,
        false,
        VERSION_293,
        BILLING_SYSTEM_293,
        native_response,
        None,
    )
    .await;
    assert_eq!(gateway.request_line, expected_line);
    assert_absent_native_headers(&gateway, "gateway route");

    // Control C: the same Native metadata builder is inert for OpenAI Chat.
    let openai = capture_openai_with_anthropic_metadata().await;
    assert_eq!(openai.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_absent_native_headers(&openai, "non-Anthropic protocol");

    let expected_headers =
        include_bytes!("fixtures/http-layout-293/native-http1-request-headers.raw");
    let header_mismatch = raw_header_mismatch(expected_headers, &native, 54086);
    let mut mismatches = Vec::new();
    mismatches.extend(request_line_mismatch);
    mismatches.extend(header_names_mismatch);
    mismatches.extend(header_mismatch);
    if let Some(body_mismatch) = body_mismatch {
        mismatches.push(body_mismatch);
    }
    if !mismatches.is_empty() {
        let artifact_dir = save_failed_raw_capture(VERSION_293, &native)
            .expect("save failed Native 2.1.293 raw request capture");
        panic!(
            "Native 2.1.293 raw HTTP request mismatch: {}; captured request-line/header/body at {}",
            mismatches.join("; "),
            artifact_dir.display()
        );
    }
}

#[tokio::test]
async fn native_295_http_layout_matches_its_own_raw_header_body_and_response() {
    let _serial = HTTP_LAYOUT_ENV_LOCK.lock().await;
    let _key = EnvRestore::set(KEY_ENV, Some(DUMMY_KEY));
    let native_response = include_bytes!("fixtures/http-layout-295/native-offered-response.raw");
    let native = capture_anthropic(
        true,
        true,
        true,
        VERSION_295,
        BILLING_SYSTEM_295,
        native_response,
        Some(58855),
    )
    .await;

    assert_eq!(native.request_line, "POST /v1/messages?beta=true HTTP/1.1");
    let expected_headers =
        include_bytes!("fixtures/http-layout-295/native-http1-request-headers.raw");
    let header_mismatch = raw_header_mismatch(expected_headers, &native, 58855);

    let native_body = include_bytes!("fixtures/http-layout-295/native-request-body.raw.json");
    let expected_body = native_body_with_fixed_device_id(native_body);
    let body_mismatch = if native.body == expected_body {
        None
    } else {
        Some(raw_difference("body", &expected_body, &native.body))
    };

    let mut mismatches = Vec::new();
    mismatches.extend(header_mismatch);
    mismatches.extend(body_mismatch);
    if !mismatches.is_empty() {
        let artifact_dir = save_failed_raw_capture(VERSION_295, &native)
            .expect("save failed Native 2.1.295 raw request capture");
        panic!(
            "Native 2.1.295 raw HTTP request mismatch: {}; captured request-line/header/body at {}",
            mismatches.join("; "),
            artifact_dir.display()
        );
    }
}
