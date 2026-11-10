//! Real CLI-configured HTTP plus caller-owned SDK collision replay. The native
//! receipt includes the SDK handshake, every notification ACK and both HTTP
//! sessions; the fixture is not an acknowledgement-only registry simulation.
use super::*;
use crate::desktop::{DesktopConfig, DesktopMcpServices, DesktopSessionComposition};
use crate::headless::control_plane::StdioControlPlane;
use crate::headless::io::Output;
use crate::headless::sdk_mcp_transport::SdkMcpTransport;
use crate::headless::stream_json::{OutboundMsg, StreamJsonStream};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

struct HttpPeer {
    origin: String,
    frames: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HttpPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HttpPeer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let frames = Arc::new(Mutex::new(Vec::new()));
        let captured = frames.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let (header_end, content_len) = loop {
                        let mut chunk = [0; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                        if let Some(at) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..at]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            break (at + 4, length);
                        }
                    };
                    while bytes.len() < header_end + content_len {
                        let mut chunk = [0; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(count, 0);
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    let post = headers.starts_with("POST ");
                    let (status, body) = if post {
                        let frame: Value =
                            serde_json::from_slice(&bytes[header_end..header_end + content_len])
                                .unwrap();
                        captured.lock().unwrap().push(frame.clone());
                        match frame.get("id") {
                            None => ("202 Accepted", String::new()),
                            Some(id) => {
                                let response = if frame["method"] == "server/discover" {
                                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
                                } else {
                                    let result = match frame["method"].as_str().unwrap() {
                                        "initialize" => {
                                            json!({"protocolVersion":frame["params"]["protocolVersion"],"capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"fake-local-staging","version":"1"}})
                                        }
                                        "tools/list" => {
                                            json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}},{"name":"files","inputSchema":{"type":"object"}}]})
                                        }
                                        "tools/call" => {
                                            json!({"content":[{"type":"text","text":"fake peer complete"}],"structuredContent":{"ok":true}})
                                        }
                                        _ => json!({}),
                                    };
                                    json!({"jsonrpc":"2.0","id":id,"result":result})
                                };
                                ("200 OK", response.to_string())
                            }
                        }
                    } else {
                        ("405 Method Not Allowed", String::new())
                    };
                    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                });
            }
        });
        Self {
            origin,
            frames,
            task,
        }
    }
}

async fn build_runtime(
    root: &Path,
    peer: &HttpPeer,
) -> (
    crate::desktop::DesktopRuntime,
    Arc<StdioControlPlane>,
    mpsc::UnboundedReceiver<OutboundMsg>,
) {
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut config = DesktopConfig::default();
    config.cwd = cwd.clone();
    config.lingxi_home = root.join("home");
    config.isolated_credential_storage = true;
    config.use_noop_permission_gate = true;
    config.composition = Some(DesktopSessionComposition::HeadlessCli);
    config.cli_mcp_servers = ::mcp::json_config::parse_mcp_json_string(
        &json!({"operator":{"type":"http","url":format!("{}/mcp",peer.origin)}}).to_string(),
        ::mcp::ConfigScope::Dynamic,
    )
    .unwrap();
    let prepared =
        crate::headless::session::prepare(&mut config, &crate::headless::SessionStart::New, false)
            .await
            .unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
        tokio::io::sink(),
    )));
    let (tx, outbound) = mpsc::unbounded_channel();
    let plane = StdioControlPlane::new(Arc::new(tx));
    let factory_plane = plane.clone();
    config.mcp_services_factory = Some(Arc::new(move |base| {
        let transport = SdkMcpTransport::new(base, factory_plane.clone(), cwd.clone());
        DesktopMcpServices {
            transport: transport.clone(),
            raw_connections: transport,
        }
    }));
    let runtime = crate::headless::session::build_prepared(
        config,
        prepared,
        stream,
        Arc::new(crate::headless::HeadlessPermissionRequestSink),
    )
    .await
    .unwrap();
    (runtime, plane, outbound)
}

fn case<'a>(fixture: &'a Value, name: &str) -> &'a Value {
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap()
}
fn assert_reply(fixture: &Value, name: &str, body: Json, origin: &str) {
    let actual = Json::object(vec![
        ("subtype".into(), "success".into()),
        ("request_id".into(), name.into()),
        ("response".into(), body),
    ])
    .unwrap()
    .to_json_string()
    .unwrap()
    .replace(origin, "http://127.0.0.1:PORT");
    assert_eq!(
        actual,
        case(fixture, name)["responseJson"].as_str().unwrap(),
        "native {name}"
    );
}
async fn set_servers(
    runtime: &crate::desktop::DesktopRuntime,
    transport: &SdkMcpTransport,
    source: Json,
) -> (Json, Vec<::mcp::McpServerConfig>) {
    let parsed =
        crate::headless::sdk_mcp_server_controls::parse_set_servers(&source, false).unwrap();
    let retire = |keys: &[String]| transport.retire_server_keys(keys);
    runtime
        .orchestrator
        .set_mcp_servers_from_control(parsed.configs, parsed.errors, false, &[], &retire)
        .await
        .unwrap()
}
async fn next_sdk(outbound: &mut mpsc::UnboundedReceiver<OutboundMsg>) -> Json {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), outbound.recv())
            .await
            .unwrap()
            .unwrap();
        if let OutboundMsg::Line(line) = message {
            let source = Json::parse(&line).unwrap();
            if source.string_at("/request/subtype") == Some("mcp_message") {
                return source;
            }
        }
    }
}
async fn ack(plane: &StdioControlPlane, message: &Json, response: Option<Json>) {
    let body = response
        .map(|response| Json::object(vec![("mcp_response".into(), response)]).unwrap())
        .unwrap_or_else(|| Json::parse("{}").unwrap());
    let reply = Json::object(vec![
        ("type".into(), "control_response".into()),
        (
            "response".into(),
            Json::object(vec![
                ("subtype".into(), "success".into()),
                (
                    "request_id".into(),
                    message.subprojection("/request_id").unwrap(),
                ),
                ("response".into(), body),
            ])
            .unwrap(),
        ),
    ])
    .unwrap();
    plane.resolve_response(&reply).await;
}

#[tokio::test]
async fn native_322_cli_http_and_sdk_shadow_replay_real_handshakes_and_namespace() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/mcp_sdk_cli_shadow_2_1_322.json"
    ))
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let peer = HttpPeer::start().await;
    let (runtime, plane, mut outbound) = build_runtime(root.path(), &peer).await;
    let transport = plane.sdk_mcp_transport().unwrap();
    let original = runtime
        .mcp_registry
        .get_client_by_key("operator")
        .await
        .unwrap();
    assert!(
        runtime
            .mcp_registry
            .get_config_by_key("operator")
            .await
            .unwrap()
            .metadata
            .cli_owned
    );
    for name in ["empty", "before"] {
        let request = Json::from_serde_value(&case(&fixture, name)["request"]).unwrap();
        let body = if name == "empty" {
            set_servers(&runtime, &transport, request).await.0
        } else {
            runtime.orchestrator.sdk_mcp_status().await.unwrap()
        };
        assert_reply(&fixture, name, body, &peer.origin);
    }
    let request = Json::from_serde_value(&case(&fixture, "replace")["request"]).unwrap();
    let (body, pending) = set_servers(&runtime, &transport, request).await;
    assert_reply(&fixture, "replace", body, &peer.origin);
    assert_eq!(pending.len(), 1);
    assert_ne!(
        pending[0].name, "operator",
        "SDK uses its independent private slot"
    );
    assert_eq!(pending[0].authored_name().as_str(), "operator");
    assert_reply(
        &fixture,
        "status",
        runtime.orchestrator.sdk_mcp_status().await.unwrap(),
        &peer.origin,
    );
    assert!(
        !runtime
            .orchestrator
            .sdk_mcp_message_is_connected(&"operator".encode_utf16().collect::<Vec<_>>())
            .await
    );
    transport.configure_server_names(&pending);
    let owner = runtime.orchestrator.clone();
    let connect = tokio::spawn(async move {
        owner.connect_control_mcp_servers(pending).await;
    });
    let initialize = next_sdk(&mut outbound).await;
    assert_eq!(
        initialize.string_at("/request/server_name"),
        Some("operator")
    );
    assert_eq!(
        initialize.string_at("/request/message/method"),
        Some("initialize")
    );
    assert_eq!(initialize.number_at("/request/message/id"), Some(0.0));
    assert_eq!(
        initialize
            .subprojection("/request/message/params/capabilities")
            .unwrap()
            .to_json_string()
            .unwrap(),
        "{}"
    );
    ack(&plane,&initialize,Some(Json::parse(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"fake-sdk-shadow","version":"1"}}}"#).unwrap())).await;
    let initialized = next_sdk(&mut outbound).await;
    assert_eq!(
        initialized
            .subprojection("/request/message")
            .unwrap()
            .to_json_string()
            .unwrap(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
    );
    assert!(!connect.is_finished());
    assert!(
        !runtime
            .orchestrator
            .sdk_mcp_message_is_connected(&"operator".encode_utf16().collect::<Vec<_>>())
            .await,
        "outer initialized ACK is required for own SDK delivery"
    );
    ack(&plane, &initialized, None).await;
    let list = next_sdk(&mut outbound).await;
    assert_eq!(
        list.subprojection("/request/message")
            .unwrap()
            .to_json_string()
            .unwrap(),
        r#"{"method":"tools/list","jsonrpc":"2.0","id":1}"#
    );
    ack(&plane,&list,Some(Json::parse(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"sdk_echo","description":"Fake SDK overlay","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}]}}"#).unwrap())).await;
    tokio::time::timeout(Duration::from_secs(5), connect)
        .await
        .unwrap()
        .unwrap();
    assert_reply(
        &fixture,
        "settled-status-2",
        runtime.orchestrator.sdk_mcp_status().await.unwrap(),
        &peer.origin,
    );
    assert!(Arc::ptr_eq(
        &original,
        &runtime.mcp_registry.get_client("operator").await.unwrap()
    ));
    assert!(
        runtime
            .orchestrator
            .sdk_mcp_message_is_connected(&"operator".encode_utf16().collect::<Vec<_>>())
            .await
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !runtime
            .registered_tool_names()
            .contains(&"mcp__operator__sdk_echo".to_owned())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!runtime
        .registered_tool_names()
        .contains(&"mcp__operator__echo".to_owned()));
    let request = Json::from_serde_value(&case(&fixture, "echo")["request"]).unwrap();
    let result = execute(
        &runtime.orchestrator,
        "mcp__operator__echo",
        &request,
        CallEnvironment {
            essential_traffic_only: true,
            program_result: false,
            checked: false,
            file_store: None,
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_reply(&fixture, "echo", result, &peer.origin);
    // A connected same-name SDK receives only its own peer request. It never
    // hijacks the public ordinary client's transport or the direct-call route.
    transport
        .deliver(
            "operator",
            &Json::parse(r#"{"jsonrpc":"2.0","method":"ping","id":702}"#).unwrap(),
        )
        .unwrap();
    let pong = next_sdk(&mut outbound).await;
    assert_eq!(
        pong.subprojection("/request/message")
            .unwrap()
            .to_json_string()
            .unwrap(),
        r#"{"result":{},"jsonrpc":"2.0","id":702}"#
    );
    ack(&plane, &pong, None).await;
    for name in [
        "sdk-tool-update",
        "repeat-sdk",
        "remove-sdk-shadow",
        "add-http-shadow",
        "repeat-http-shadow",
        "remove-http-shadow",
    ] {
        let raw = serde_json::to_string(&case(&fixture, name)["request"])
            .unwrap()
            .replace("http://127.0.0.1:PORT", &peer.origin);
        let (body, pending) = set_servers(&runtime, &transport, Json::parse(&raw).unwrap()).await;
        assert!(
            pending.is_empty(),
            "{name} does not create a second SDK client"
        );
        assert_reply(&fixture, name, body, &peer.origin);
        assert_reply(
            &fixture,
            &format!("{name}-status"),
            runtime.orchestrator.sdk_mcp_status().await.unwrap(),
            &peer.origin,
        );
        if name == "sdk-tool-update" {
            assert!(
                runtime
                    .mcp_registry
                    .sdk_control_config(&"operator".encode_utf16().collect::<Vec<_>>())
                    .await
                    .unwrap()
                    .metadata
                    .sdk_connector_id
                    .is_none(),
                "first-registration connector metadata survives updates"
            );
        }
    }
    let actual = peer
        .frames
        .lock()
        .unwrap()
        .iter()
        .map(|frame| {
            json!([
                frame["method"],
                frame.get("id").cloned().unwrap_or(Value::Null)
            ])
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        fixture["httpMethodIds"].as_array().unwrap().clone(),
        "SDK catalog updates must not refetch the ordinary catalog"
    );
    assert!(runtime
        .mcp_registry
        .control_candidates(&"operator".encode_utf16().collect::<Vec<_>>(), false)
        .await
        .is_empty());
}
