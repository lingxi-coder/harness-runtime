//! Real JSON-RPC/HTTP protocol-era negotiation tests.
//!
//! These tests intentionally drive `PosixMcpTransport` through an axum mock,
//! rather than testing only the pure envelope helpers. This catches probe
//! redial, compatibility fallback, corrective retry, and the live modern
//! request/result contract together.

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use lingxi_core::host::{McpConnectOptions, McpProtocolEra, McpTransport, McpTransportSpec};
use platform_posix::mcp::PosixMcpTransport;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy)]
enum DiscoveryMode {
    Modern,
    LegacyFallback,
    CorrectiveRetry,
    InvalidResult,
    Auth,
    RemoteTimeout,
    UnknownError,
    RepeatedCorrective,
    FutureOnly,
    MissingCapabilities,
    UnsupportedLegacy,
    OlderLegacy,
    Malformed,
}

#[derive(Clone)]
struct MockState {
    mode: DiscoveryMode,
    requests: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<HeaderMap>>>,
    discover_calls: Arc<std::sync::atomic::AtomicUsize>,
}

async fn handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    state.headers.lock().unwrap().push(headers);
    if matches!(state.mode, DiscoveryMode::Malformed) && request["method"] == "server/discover" {
        state.requests.lock().unwrap().push(request);
        return "invalid JSON".into_response();
    }
    let discovery = request["method"] == "server/discover";
    let reply = reply_handler(State(state), Json(request)).await;
    let mut response = reply.into_response();
    if discovery {
        response.headers_mut().insert(
            "mcp-session-id",
            HeaderValue::from_static("discovered-session"),
        );
    }
    response
}

fn discovery_result() -> Value {
    json!({
        "supportedVersions": ["2025-11-25", "2026-07-28"],
        "capabilities": { "tools": {}, "prompts": {}, "extensions": {"vendor/custom": {"enabled": true}} },
        "instructions": "Use this server for project records.",
        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "discovery-mock", "version": "286"}},
        "resultType": "complete"
    })
}

async fn reply_handler(State(state): State<MockState>, Json(request): Json<Value>) -> Json<Value> {
    state.requests.lock().unwrap().push(request.clone());
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "server/discover" => {
            if matches!(state.mode, DiscoveryMode::RemoteTimeout) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let attempt = state
                .discover_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match state.mode {
                DiscoveryMode::Modern | DiscoveryMode::InvalidResult => discovery_result(),
                DiscoveryMode::MissingCapabilities => json!({"supportedVersions": ["2026-07-28"]}),
                DiscoveryMode::LegacyFallback
                | DiscoveryMode::UnsupportedLegacy
                | DiscoveryMode::OlderLegacy
                | DiscoveryMode::Malformed => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method not found" }
                    }));
                }
                DiscoveryMode::UnknownError => {
                    return Json(
                        json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32123,"message":"not yet available"}}),
                    )
                }
                DiscoveryMode::FutureOnly => {
                    return Json(
                        json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32022,"message":"unsupported", "data":{"supported":["2027-01-01"]}}}),
                    )
                }
                DiscoveryMode::CorrectiveRetry | DiscoveryMode::RepeatedCorrective
                    if attempt == 0 || matches!(state.mode, DiscoveryMode::RepeatedCorrective) =>
                {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32022,
                            "message": "unsupported revision",
                            "data": { "supported": ["2026-07-28"] }
                        }
                    }));
                }
                DiscoveryMode::CorrectiveRetry | DiscoveryMode::RepeatedCorrective => {
                    discovery_result()
                }
                DiscoveryMode::Auth => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32001,
                            "message": "HTTP 401",
                            "data": { "httpStatus": 401 }
                        }
                    }));
                }
                DiscoveryMode::RemoteTimeout => json!({}),
            }
        }
        "initialize" => json!({
            "protocolVersion": match state.mode { DiscoveryMode::UnsupportedLegacy => "2026-07-28", DiscoveryMode::OlderLegacy => "2024-11-05", _ => "2025-11-25" },
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "protocol-mock", "version": "1" }
        }),
        "tools/list" => {
            let mut result = json!({
                "tools": [{
                    "name": "echo",
                    "description": "echo",
                    "inputSchema": { "type": "object" }
                }]
            });
            if !matches!(state.mode, DiscoveryMode::LegacyFallback) {
                result["resultType"] = json!("complete");
                result["ttlMs"] = json!(0);
                result["cacheScope"] = json!("private");
            }
            if matches!(state.mode, DiscoveryMode::InvalidResult) {
                result["resultType"] = json!("partial");
            }
            result
        }
        _ => json!({}),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn spawn_mock(mode: DiscoveryMode) -> (String, MockState) {
    let state = MockState {
        mode,
        requests: Arc::new(Mutex::new(Vec::new())),
        headers: Arc::new(Mutex::new(Vec::new())),
        discover_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/mcp", post(handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), state)
}

fn spec(url: String) -> McpTransportSpec {
    McpTransportSpec::Http {
        url,
        headers: lingxi_core::host::McpHeaders::new(),
        headers_helper: None,
        oauth: None,
    }
}

fn options() -> McpConnectOptions {
    options_with_deadline(5_000)
}

fn options_with_deadline(deadline_ms: u64) -> McpConnectOptions {
    McpConnectOptions {
        expected_era: Some(McpProtocolEra::Modern),
        deadline_ms,
        probe_timeout_ms: Some(3_000),
        elicitation: lingxi_core::host::McpElicitationCapabilities::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modern_success_injects_meta_and_strips_complete_result_type() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::Modern).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect("modern handshake");
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    assert_eq!(result.negotiated.version, "2026-07-28");

    let tools = transport
        .list_tools(&result.connection)
        .await
        .expect("tools/list");
    assert_eq!(tools.len(), 1);
    let requests = state.requests.lock().unwrap().clone();
    let discover = requests
        .iter()
        .find(|r| r["method"] == "server/discover")
        .unwrap();
    assert_eq!(discover["params"]["_meta"].as_object().unwrap().len(), 3);
    assert_eq!(discover["params"].as_object().unwrap().len(), 1);
    assert_eq!(discover["id"], "server-discover-probe-1");
    assert!(!requests.iter().any(|request| matches!(
        request["method"].as_str(),
        Some("initialize" | "notifications/initialized")
    )));
    let metadata = transport
        .server_metadata(result.connection.connection_id)
        .unwrap();
    assert_eq!(
        metadata.server_info.unwrap(),
        json!({"name":"discovery-mock", "version":"286"})
    );
    assert_eq!(
        metadata.instructions.as_deref(),
        Some("Use this server for project records.")
    );
    assert_eq!(metadata.discovery.unwrap()["cacheScope"], "private");
    assert!(result.capabilities.prompts);
    assert_eq!(
        result.capabilities.extensions["vendor/custom"]["enabled"],
        true
    );
    let headers = state.headers.lock().unwrap();
    assert!(headers[0].get("mcp-session-id").is_none());
    assert_eq!(headers[0]["mcp-protocol-version"], "2026-07-28");
    assert_eq!(headers[0]["mcp-method"], "server/discover");
    assert_eq!(headers[1]["mcp-session-id"], "discovered-session");
    assert_eq!(headers[1]["mcp-protocol-version"], "2026-07-28");
    drop(headers);
    let list = requests
        .iter()
        .find(|r| r["method"] == "tools/list")
        .unwrap();
    assert_eq!(list["params"]["_meta"].as_object().unwrap().len(), 3);
    assert_eq!(list["id"], 1, "probe must not advance live request IDs");
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_not_found_falls_back_in_place_to_legacy() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::LegacyFallback).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect("legacy fallback");
    assert_eq!(result.negotiated.era, McpProtocolEra::Legacy);
    transport
        .list_tools(&result.connection)
        .await
        .expect("legacy tools/list");
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "server/discover")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "initialize")
            .count(),
        1,
        "legacy fallback must initialize the same live connection"
    );
    let list = requests
        .iter()
        .find(|r| r["method"] == "tools/list")
        .unwrap();
    assert!(list["params"].get("_meta").is_none());
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_revision_gets_one_corrective_retry() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::CorrectiveRetry).await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        transport.connect_and_initialize(&spec(url), options()),
    )
    .await
    .expect("corrective retry deadline")
    .expect("corrective retry handshake");
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    assert_eq!(
        state
            .discover_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_probe_timeout_is_not_treated_as_compatibility() {
    let transport = PosixMcpTransport::new();
    let (url, _state) = spawn_mock(DiscoveryMode::RemoteTimeout).await;
    let error = transport
        .connect_and_initialize(&spec(url), options_with_deadline(100))
        .await
        .expect_err("remote timeout must be reported");
    assert!(
        matches!(&error, lingxi_core::host::McpError::Internal(message) if message.contains("timed out")),
        "unexpected remote timeout classification: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_network_failure_is_not_treated_as_compatibility() {
    let transport = PosixMcpTransport::new();
    let error = transport
        .connect_and_initialize(
            &spec("http://127.0.0.1:9/mcp".to_string()),
            options_with_deadline(100),
        )
        .await
        .expect_err("remote network failure must be reported");
    assert!(
        matches!(&error, lingxi_core::host::McpError::Connection(message) if message.contains("probe failed")),
        "unexpected remote network classification: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_auth_error_is_not_treated_as_compatibility() {
    let transport = PosixMcpTransport::new();
    let (url, _state) = spawn_mock(DiscoveryMode::Auth).await;
    let error = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect_err("remote auth must be reported");
    assert!(
        matches!(
            error,
            lingxi_core::host::McpError::HttpResponse { status: 401, .. }
        ),
        "unexpected remote auth classification: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modern_catalog_rejects_non_complete_result_type() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::InvalidResult).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect("modern initialize");
    let error = transport
        .list_tools(&result.connection)
        .await
        .expect_err("partial modern catalog result must fail");
    assert!(
        matches!(&error, lingxi_core::host::McpError::Result(error) if error.code=="UNSUPPORTED_RESULT_TYPE"),
        "unexpected modern result classification: {error:?}"
    );
    assert!(state.requests.lock().unwrap().iter().any(|request| {
        request["method"] == "tools/list"
            && request["params"]["_meta"]
                .as_object()
                .is_some_and(|meta| meta.len() == 3)
    }));
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_discovery_and_ordinary_errors_fall_back_on_the_same_http_session() {
    for mode in [
        DiscoveryMode::MissingCapabilities,
        DiscoveryMode::UnknownError,
        DiscoveryMode::Malformed,
    ] {
        let transport = PosixMcpTransport::new();
        let (url, state) = spawn_mock(mode).await;
        let result = transport
            .connect_and_initialize(&spec(url), options())
            .await
            .unwrap();
        assert_eq!(result.negotiated.era, McpProtocolEra::Legacy);
        let requests = state.requests.lock().unwrap().clone();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            1
        );
        let initialization = requests
            .iter()
            .position(|r| r["method"] == "initialize")
            .unwrap();
        if !matches!(mode, DiscoveryMode::Malformed) {
            assert_eq!(
                state.headers.lock().unwrap()[initialization]["mcp-session-id"],
                "discovered-session"
            );
        }
        assert_eq!(
            requests[initialization]["params"]["protocolVersion"],
            "2025-11-25"
        );
        transport
            .disconnect(result.connection.connection_id)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_modern_revisions_and_repeated_correction_are_fatal() {
    for (mode, attempts) in [
        (DiscoveryMode::FutureOnly, 1),
        (DiscoveryMode::RepeatedCorrective, 2),
    ] {
        let transport = PosixMcpTransport::new();
        let (url, state) = spawn_mock(mode).await;
        let error = transport
            .connect_and_initialize(&spec(url), options())
            .await
            .unwrap_err();
        assert!(matches!(error, lingxi_core::host::McpError::Handshake(_)));
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), attempts);
        assert!(requests
            .iter()
            .all(|request| request["method"] == "server/discover"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_selection_is_validated_before_initialized_and_actual_revision_retained() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::UnsupportedLegacy).await;
    let error = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .unwrap_err();
    assert!(matches!(error, lingxi_core::host::McpError::Handshake(_)));
    assert!(!state
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| request["method"] == "notifications/initialized"));
    let (url, state) = spawn_mock(DiscoveryMode::OlderLegacy).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .unwrap();
    assert_eq!(result.negotiated.version, "2024-11-05");
    transport.list_tools(&result.connection).await.unwrap();
    let headers = state.headers.lock().unwrap();
    assert_eq!(
        headers.last().unwrap()["mcp-protocol-version"],
        "2024-11-05"
    );
    drop(headers);
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[test]
fn discovery_and_rpc_error_decisions_match_pinned_upstream_execution() {
    use platform_common::mcp_remote::{
        modern_probe_error, parse_modern_discovery, ModernProbeErrorAction,
    };
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/mcp_negotiation_286_oracle.json")).unwrap();
    let mut checked = 0;
    for case in fixture["discoveryCases"].as_array().unwrap() {
        // These two inputs exercise configurable future revision/pin surfaces
        // not exposed by this host, whose supported modern revision is fixed.
        if case.get("context").is_some() {
            continue;
        }
        let result = parse_modern_discovery(case["result"].clone());
        match case["expected"]["kind"].as_str().unwrap() {
            "modern" => {
                let result = result.unwrap_or_else(|| panic!("{}", case["name"]));
                assert_eq!(
                    result.version, case["expected"]["version"],
                    "{}",
                    case["name"]
                );
                assert_eq!(
                    result.metadata.discovery.unwrap(),
                    case["expected"]["discover"],
                    "{}",
                    case["name"]
                );
            }
            "legacy" => assert!(result.is_none(), "{}", case["name"]),
            other => panic!("unexpected oracle verdict {other}"),
        }
        checked += 1;
    }
    assert_eq!(checked, 23);
    let mut errors_checked = 0;
    for case in fixture["decisionCases"].as_array().unwrap() {
        if case.get("context").is_some()
            || !matches!(
                case["event"]["kind"].as_str(),
                Some("rpc-error" | "http-error")
            )
        {
            continue;
        }
        let event = &case["event"];
        let error = if event["kind"] == "http-error" {
            jsonrpc::ResponseError {
                code: -32001,
                message: "HTTP error".into(),
                data: Some(json!({"httpStatus":event["status"], "body": event["body"]})),
            }
        } else {
            jsonrpc::ResponseError {
                code: event["code"].as_i64().unwrap().try_into().unwrap(),
                message: event["message"].as_str().unwrap().into(),
                data: event.get("data").cloned(),
            }
        };
        let actual = match modern_probe_error(&error, false) {
            Ok(ModernProbeErrorAction::Legacy) => "legacy",
            Ok(ModernProbeErrorAction::Retry) => "corrective",
            Err(_) => "error",
        };
        assert_eq!(actual, case["expected"]["kind"], "{}", case["name"]);
        errors_checked += 1;
    }
    assert!(errors_checked >= 13);
}

const MODERN_STDIO_SERVER: &str = r#"
import json, os, sys
mode, logfile = sys.argv[1:]
pid = os.getpid()
initialized = False
if os.path.exists(logfile):
    with open(logfile) as log: previous = [json.loads(line) for line in log]
    for row in previous:
        if row.get('request', {}).get('method') == 'server/discover':
            try: os.kill(row['pid'], 0); alive = True
            except ProcessLookupError: alive = False
            with open(logfile, 'a') as log: log.write(json.dumps({'event':'probe_alive_at_live_start','alive':alive})+'\n')
            break
with open(logfile, 'a') as log: log.write(json.dumps({'event':'start','pid':pid})+'\n')
for line in sys.stdin:
    req = json.loads(line)
    with open(logfile, 'a') as log: log.write(json.dumps({'pid':pid,'request':req})+'\n')
    method = req['method']
    if 'id' not in req: continue
    reply = {'jsonrpc':'2.0','id':req['id']}
    if method == 'server/discover':
        if mode != 'modern' and not initialized: continue
        reply['result'] = {'supportedVersions':['2026-07-28'],'capabilities':{'tools':{}},'resultType':'complete','instructions':'stdio instructions','_meta':{'io.modelcontextprotocol/serverInfo':{'name':'stdio-discovery','version':'1'}}}
    elif method == 'initialize':
        initialized = True
        reply['error'] = {'code':-32022,'message':'modern only','data':{'supported':['2027-01-01' if mode == 'incompatible' else '2026-07-28']}}
    elif method == 'tools/list':
        reply['result'] = {'tools':[],'resultType':'complete','ttlMs':0,'cacheScope':'private'}
    else: reply['result'] = {}
    print(json.dumps(reply), flush=True)
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_modern_uses_disposable_sibling_and_no_live_initialize() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("requests.jsonl");
    let transport = PosixMcpTransport::new();
    let spec = McpTransportSpec::Stdio {
        command: "python3".into(),
        args: vec![
            "-u".into(),
            "-c".into(),
            MODERN_STDIO_SERVER.into(),
            "modern".into(),
            log.to_string_lossy().into_owned(),
        ],
        env: Default::default(),
    };
    let result = transport
        .connect_and_initialize(&spec, options())
        .await
        .unwrap();
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    transport.list_tools(&result.connection).await.unwrap();
    let rows: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("request").is_some())
        .collect();
    assert_eq!(
        requests.len(),
        2,
        "only probe and live tools/list are allowed: {rows:?}"
    );
    assert_eq!(requests[0]["request"]["method"], "server/discover");
    assert_eq!(requests[0]["request"]["id"], "server-discover-probe-1");
    assert_eq!(requests[1]["request"]["method"], "tools/list");
    assert_ne!(requests[0]["pid"], requests[1]["pid"]);
    assert!(
        rows.iter()
            .any(|row| row["event"] == "probe_alive_at_live_start" && row["alive"] == false),
        "probe must be reaped before live process starts: {rows:?}"
    );
    assert_eq!(
        transport
            .server_metadata(result.connection.connection_id)
            .unwrap()
            .instructions
            .as_deref(),
        Some("stdio instructions")
    );
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
    assert!(transport
        .server_metadata(result.connection.connection_id)
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_timeout_rediscovery_only_recovers_a_mutual_modern_revision() {
    for mode in ["slow", "incompatible"] {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("requests.jsonl");
        let transport = PosixMcpTransport::new();
        let spec = McpTransportSpec::Stdio {
            command: "python3".into(),
            args: vec![
                "-u".into(),
                "-c".into(),
                MODERN_STDIO_SERVER.into(),
                mode.into(),
                log.to_string_lossy().into_owned(),
            ],
            env: Default::default(),
        };
        let result = transport
            .connect_and_initialize(
                &spec,
                McpConnectOptions {
                    probe_timeout_ms: Some(150),
                    ..options()
                },
            )
            .await;
        let rows: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let live_requests: Vec<&Value> = rows
            .iter()
            .filter(|row| {
                row["request"]["method"] == "initialize"
                    || row["request"]["method"] == "server/discover"
            })
            .collect();
        if mode == "slow" {
            let result = result.unwrap();
            assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
            assert_eq!(live_requests.len(), 3, "{rows:?}");
            assert_eq!(live_requests[1]["pid"], live_requests[2]["pid"]);
            assert_eq!(live_requests[2]["request"]["method"], "server/discover");
            transport
                .disconnect(result.connection.connection_id)
                .await
                .unwrap();
        } else {
            assert!(result.is_err());
            assert_eq!(
                live_requests.len(),
                2,
                "unsupported modern version must not rediscover: {rows:?}"
            );
        }
    }
}
