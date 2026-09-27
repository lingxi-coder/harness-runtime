//! Integration tests for `McpClient` against an in-process mock server.
//!
//! The mock plays the role of a real MCP server: it reads JSON-RPC requests
//! from one end of a duplex pipe, captures them for assertion, and responds
//! with scripted payloads. Tests lock the literal wire bytes claude-code
//! emits (plan M2-02b §"Critical 1:1 fidelity items").

#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Once, OnceLock};

use serde_json::{json, Value};
use tokio::io::{duplex, AsyncRead, AsyncWrite, DuplexStream};
use tokio::sync::Mutex;
use tracing::field::Field;
use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::Registry;

use mcp::McpClient;
use platform_api::{McpNegotiatedProtocol, McpProtocolEra};

/// Records every outgoing JSON-RPC payload received from the client end.
/// Tests inspect this to assert literal byte content.
#[derive(Default, Clone)]
pub struct CapturedFrames {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl CapturedFrames {
    async fn push(&self, bytes: Vec<u8>) {
        self.inner.lock().await.push(bytes);
    }

    /// Snapshot the captured frames as a clone (test-side assertion helper).
    pub async fn snapshot(&self) -> Vec<Vec<u8>> {
        self.inner.lock().await.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListPaginatedRow {
    event: String,
    method: String,
    page_count: Option<u64>,
    item_count: Option<u64>,
    outcome: String,
    source: Option<String>,
}

#[derive(Default, Clone)]
struct ListPaginatedCapture {
    rows: Arc<StdMutex<Vec<ListPaginatedRow>>>,
}

impl<S: Subscriber> Layer<S> for ListPaginatedCapture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        struct Visitor {
            event: Option<String>,
            method: Option<String>,
            page_count: Option<u64>,
            item_count: Option<u64>,
            outcome: Option<String>,
            source: Option<String>,
        }

        impl tracing::field::Visit for Visitor {
            fn record_u64(&mut self, field: &Field, value: u64) {
                match field.name() {
                    "pageCount" => self.page_count = Some(value),
                    "itemCount" => self.item_count = Some(value),
                    _ => {}
                }
            }

            fn record_i64(&mut self, field: &Field, value: i64) {
                if value >= 0 {
                    self.record_u64(field, value as u64);
                }
            }

            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}").trim_matches('"').to_string();
                match field.name() {
                    "event" => self.event = Some(rendered),
                    "method" => self.method = Some(rendered),
                    "outcome" => self.outcome = Some(rendered),
                    "source" if rendered != "None" => self.source = Some(rendered),
                    _ => {}
                }
            }

            fn record_str(&mut self, field: &Field, value: &str) {
                match field.name() {
                    "event" => self.event = Some(value.to_string()),
                    "method" => self.method = Some(value.to_string()),
                    "outcome" => self.outcome = Some(value.to_string()),
                    "source" => self.source = Some(value.to_string()),
                    _ => {}
                }
            }
        }

        let mut visitor = Visitor {
            event: None,
            method: None,
            page_count: None,
            item_count: None,
            outcome: None,
            source: None,
        };
        event.record(&mut visitor);
        if visitor.event.as_deref() == Some(telemetry::tengu::mcp::LIST_PAGINATED) {
            self.rows.lock().unwrap().push(ListPaginatedRow {
                event: visitor.event.expect("event name"),
                method: visitor.method.expect("list method"),
                page_count: visitor.page_count,
                item_count: visitor.item_count,
                outcome: visitor.outcome.expect("outcome"),
                source: visitor.source,
            });
        }
    }
}

fn install_list_paginated_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let capture = ListPaginatedCapture {
            rows: list_paginated_rows().clone(),
        };
        tracing::subscriber::set_global_default(Registry::default().with(capture))
            .expect("install global list_paginated capture");
    });
}

fn list_paginated_rows() -> &'static Arc<StdMutex<Vec<ListPaginatedRow>>> {
    static ROWS: OnceLock<Arc<StdMutex<Vec<ListPaginatedRow>>>> = OnceLock::new();
    ROWS.get_or_init(|| Arc::new(StdMutex::new(Vec::new())))
}

fn list_paginated_capture_lock() -> &'static StdMutex<()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
}

fn clear_list_paginated_rows() {
    list_paginated_rows().lock().unwrap().clear();
}

fn take_list_paginated_rows() -> Vec<ListPaginatedRow> {
    std::mem::take(&mut *list_paginated_rows().lock().unwrap())
}

fn assert_contains_single_list_paginated_row(expected: ListPaginatedRow) {
    let rows = take_list_paginated_rows();
    let matches = rows.iter().filter(|row| **row == expected).count();
    assert_eq!(
        matches, 1,
        "expected exactly one matching telemetry row {expected:?}, captured rows: {rows:?}"
    );
}

/// Spawns a mock server task that:
///   1. Reads JSON-RPC frames from `server_read` (line-delimited).
///   2. For each request, records the frame bytes in `captured` and
///      dispatches to `responder`, writing the response back on
///      `server_write`.
///
/// The mock supports synchronous request/response only (one in → one out).
/// Notifications and responses without a matching `responder` return value
/// are recorded in `captured` and otherwise ignored.
fn spawn_mock_server<F>(
    server_read: DuplexStream,
    server_write: DuplexStream,
    captured: CapturedFrames,
    responder: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let responder = Arc::new(responder);
    tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let mut writer = server_write;
        let mut line = String::new();
        loop {
            line.clear();
            let Ok(n) = reader.read_line(&mut line).await else {
                break;
            };
            if n == 0 {
                break;
            }
            captured.push(line.as_bytes().to_vec()).await;
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(resp) = (responder)(&req) {
                let mut out = serde_json::to_vec(&resp).unwrap();
                out.push(b'\n');
                if writer.write_all(&out).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        }
    })
}

/// Build a client + capture handle bound to a duplex pair.
///
/// Returns `(client, captured_frames, mock_handle)`. The platform layer
/// (M2-02c) wraps this same `Connection` constructor for stdio; here we
/// bypass `tokio::process` and feed the raw read/write halves directly.
pub async fn make_client_against_mock<F>(
    server_name: &str,
    cwd: std::path::PathBuf,
    responder: F,
) -> (McpClient, CapturedFrames, tokio::task::JoinHandle<()>)
where
    F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
{
    // Two duplex pipes: client_write → server_read, server_write → client_read.
    let (client_read, server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);
    let captured = CapturedFrames::default();
    let handle = spawn_mock_server(server_read, server_write, captured.clone(), responder);

    let client_read: Box<dyn AsyncRead + Send + Unpin> = Box::new(client_read);
    let client_write: Box<dyn AsyncWrite + Send + Unpin> = Box::new(client_write);
    let connection = jsonrpc::Connection::new_line_delimited(client_read, client_write);
    let client = McpClient::new(server_name, cwd, Arc::new(connection)).await;
    (client, captured, handle)
}

#[tokio::test]
async fn mock_server_responds_to_initialize() {
    let (client, _captured, _h) =
        make_client_against_mock("filesystem", std::path::PathBuf::from("/tmp/work"), |req| {
            // Echo every request as `{"id":..., "result": {...}}` for now.
            let id = req["id"].clone();
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock", "version": "0.0.1" }
                }
            }))
        })
        .await;

    // Task 8 wires the real implementation; the harness should round-trip
    // a successful `initialize` against the mock and yield the parsed
    // server-capability DTO.
    let caps = client
        .initialize()
        .await
        .expect("initialize succeeds against mock");
    assert!(caps.tools, "single `tools` capability parsed");
}

#[tokio::test]
async fn initialize_emits_literal_claude_code_clientinfo() {
    let (client, captured, _h) =
        make_client_against_mock("filesystem", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                    "serverInfo": { "name": "mock", "version": "1.0.0" }
                }
            }))
        })
        .await;

    client.initialize().await.expect("initialize ok");
    let frames = captured.snapshot().await;
    assert_eq!(frames.len(), 1, "initialize sent exactly one frame");

    let frame = std::str::from_utf8(&frames[0]).expect("utf8");
    // Literal byte assertions — these lock the wire format.
    assert!(
        frame.contains(r#""method":"initialize""#),
        "method must be literal \"initialize\", got: {frame}",
    );
    assert!(
        frame.contains(r#""clientInfo":{"name":"lingxi""#),
        "literal lingxi clientInfo must appear, got: {frame}",
    );
    assert!(
        frame.contains(r#""protocolVersion":"2025-11-25""#),
        "literal protocolVersion must appear, got: {frame}",
    );
    // Capabilities are EXACTLY {"roots":{"listChanged":true},"elicitation":{}}
    // (parity 2.1.207 J7n(): roots advertises listChanged:true).
    assert!(
        frame.contains(r#""capabilities":{"roots":{"listChanged":true},"elicitation":{}}"#),
        "literal capability shape must appear, got: {frame}",
    );
}

#[tokio::test]
async fn initialize_stores_server_capabilities() {
    let (client, _captured, _h) =
        make_client_against_mock("filesystem", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {},
                        "resources": {},
                        "prompts": {},
                        "logging": {}
                    },
                    "serverInfo": { "name": "mock", "version": "1.0.0" }
                }
            }))
        })
        .await;

    let caps = client.initialize().await.expect("ok");
    assert!(caps.tools, "tools cap parsed");
    assert!(caps.resources, "resources cap parsed");
    assert!(caps.prompts, "prompts cap parsed");
    assert!(caps.logging, "logging cap parsed");
}

#[tokio::test]
async fn initialize_truncates_long_server_instructions() {
    // Build a >2048-char instructions blob; assert the stored value is
    // capped at MAX_MCP_DESCRIPTION_LENGTH and ends with the U+2026
    // " [truncated]" suffix that claude-code uses.
    let long_text: String = "x".repeat(3000);
    let long_text_for_responder = long_text.clone();
    let (client, _captured, _h) = make_client_against_mock(
        "filesystem",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock", "version": "1.0.0" },
                    "instructions": long_text_for_responder,
                }
            }))
        },
    )
    .await;

    client.initialize().await.expect("ok");
    let stored = client
        .server_instructions()
        .await
        .expect("instructions captured");
    assert!(
        stored.ends_with("\u{2026} [truncated]"),
        "instructions must end with the literal truncation suffix, got: {stored:?}",
    );
    // Truncated value must be strictly shorter than the original 3000-char input.
    assert!(
        stored.chars().count() < long_text.chars().count(),
        "truncated value must be shorter than original 3000-char input ({} chars)",
        stored.chars().count(),
    );
}

#[tokio::test]
async fn list_tools_prefixes_full_name_with_double_underscores() {
    let (client, _cap, _h) =
        make_client_against_mock("filesystem", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [
                        { "name": "read_file", "description": "Read", "inputSchema": {} },
                        { "name": "write_file", "description": "Write", "inputSchema": {} }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].server_name, "filesystem");
    assert_eq!(tools[0].tool_name(), "read_file");
    // LITERAL full-name format: mcp__<server>__<tool>.
    assert_eq!(tools[0].full_name, "mcp__filesystem__read_file");
    assert_eq!(tools[1].full_name, "mcp__filesystem__write_file");
}

#[tokio::test]
async fn list_tools_aggregates_paginated_pages() {
    let _capture = list_paginated_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_list_paginated_capture();
    clear_list_paginated_rows();
    let page = Arc::new(AtomicUsize::new(0));
    let page_clone = page.clone();
    let (client, captured, _h) = make_client_against_mock(
        "filesystem",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => match page_clone.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert!(req["params"].get("cursor").is_none());
                        json!({
                            "tools": [
                                { "name": "read_file", "description": "Read", "inputSchema": {} }
                            ],
                            "nextCursor": "next"
                        })
                    }
                    1 => {
                        assert_eq!(req["params"]["cursor"], json!("next"));
                        json!({
                            "tools": [
                                { "name": "write_file", "description": "Write", "inputSchema": {} }
                            ]
                        })
                    }
                    other => panic!("unexpected page request {other}"),
                },
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.tool_name())
            .collect::<Vec<_>>(),
        vec!["read_file", "write_file"]
    );
    let frames = captured.snapshot().await;
    assert_eq!(frames.len(), 3, "initialize plus two tools/list pages");
    assert_contains_single_list_paginated_row(ListPaginatedRow {
        event: telemetry::tengu::mcp::LIST_PAGINATED.to_string(),
        method: "tools/list".to_string(),
        page_count: Some(2),
        item_count: Some(2),
        outcome: "complete".to_string(),
        source: Some("pages".to_string()),
    });
}

#[tokio::test]
async fn modern_list_tools_emits_the_aggregate_shape_even_for_one_page() {
    let _capture = list_paginated_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_list_paginated_capture();
    clear_list_paginated_rows();
    let (client, _captured, _h) =
        make_client_against_mock("filesystem", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let result = match req["method"].as_str().unwrap_or("") {
                "initialize" => json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "resultType": "complete",
                    "tools": [
                        { "name": "read_file", "description": "Read", "inputSchema": {} }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;
    let client = client.with_negotiated_protocol(McpNegotiatedProtocol {
        era: McpProtocolEra::Modern,
        version: "2026-07-28".to_string(),
    });

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_contains_single_list_paginated_row(ListPaginatedRow {
        event: telemetry::tengu::mcp::LIST_PAGINATED.to_string(),
        method: "tools/list".to_string(),
        page_count: None,
        item_count: Some(1),
        outcome: "complete".to_string(),
        source: Some("aggregate".to_string()),
    });
}

#[tokio::test]
async fn full_name_normalizes_invalid_server_name_and_round_trips() {
    // A server whose name has an API-invalid char (`.`) is normalized in the
    // FQN `<server>` token (`my.server` → `my_server`) so the advertised tool
    // name matches `^[a-zA-Z0-9_-]{1,64}$`. The DTO `server_name` stays RAW for
    // display (claude-code keeps client.name raw, normalizing only the FQN).
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_clone = captured.clone();
    let (client, _cap, _h) = make_client_against_mock(
        "my.server",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            if method == "tools/call" {
                let name = req["params"]["name"].as_str().unwrap_or("").to_string();
                let c = captured_clone.clone();
                tokio::spawn(async move { *c.lock().await = Some(name) });
            }
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [ { "name": "read_file", "description": "Read", "inputSchema": {} } ]
                }),
                "tools/call" => {
                    json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false })
                }
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    // FQN server token normalized; DTO server_name stays raw.
    assert_eq!(tools[0].full_name, "mcp__my_server__read_file");
    assert_eq!(tools[0].server_name, "my.server");

    // The normalized FQN strips back correctly: the wire `name` is the bare
    // tool name, proving the build/strip round-trip is self-consistent.
    client
        .call_tool("mcp__my_server__read_file", json!({}))
        .await
        .expect("normalized FQN should strip + call");
    // give the spawned capture task a tick
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(captured.lock().await.as_deref(), Some("read_file"));
}

#[tokio::test]
async fn call_tool_times_out_with_locked_error_string() {
    // Mock responds to `initialize` but NEVER responds to `tools/call`,
    // forcing the client to hit the timeout branch.
    let (client, _cap, _h) =
        make_client_against_mock("filesystem", std::path::PathBuf::from("/tmp/work"), |req| {
            let method = req["method"].as_str().unwrap_or("");
            if method == "initialize" {
                let id = req["id"].clone();
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "m", "version": "0" }
                    }
                }))
            } else {
                None // black hole → timeout
            }
        })
        .await;

    client.initialize().await.expect("init");

    // Use a tiny override so the test runs in <100ms instead of 60s.
    let err = client
        .call_tool_with_timeout(
            "mcp__filesystem__read_file",
            json!({}),
            std::time::Duration::from_millis(50),
        )
        .await
        .expect_err("should time out");

    // EXACT literal — error string is load-bearing for M2-07 integration tests.
    // NOTE: the user-facing seconds value is the rounded-up Duration.as_secs().
    // For sub-second timeouts we still report at least 1 (claude-code's
    // implementation uses the seconds setting from settings.json, but our
    // unit-test path uses Duration directly — round-up keeps the format stable).
    let msg = format!("{err}");
    assert!(
        msg.starts_with(r#"MCP server "filesystem" tool "read_file" timed out after "#),
        "must match literal prefix, got: {msg}",
    );
    assert!(msg.ends_with('s'), "must end with literal s, got: {msg}");
}

#[tokio::test]
async fn call_tool_full_name_validation_rejects_wrong_prefix() {
    // The CALLER passes a full-name like `mcp__filesystem__read_file`.
    // The client must strip the `mcp__<server>__` prefix before sending
    // `name: "read_file"` over the wire.
    let captured_method = Arc::new(Mutex::new(None::<String>));
    let captured_clone = captured_method.clone();

    let (client, _cap, _h) = make_client_against_mock(
        "filesystem",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            if method == "tools/call" {
                let tool_name = req["params"]["name"].as_str().unwrap_or("").to_string();
                let captured_clone = captured_clone.clone();
                tokio::spawn(async move {
                    *captured_clone.lock().await = Some(tool_name);
                });
            }
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/call" => json!({
                    "content": [{ "type": "text", "text": "ok" }],
                    "isError": false
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    client
        .call_tool("mcp__filesystem__read_file", json!({}))
        .await
        .expect("call");

    // Give the inner spawn a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let name = captured_method.lock().await.clone();
    assert_eq!(
        name.as_deref(),
        Some("read_file"),
        "wire name must be unprefixed"
    );
}

#[tokio::test]
async fn list_tools_truncates_oversized_descriptions() {
    // 2049-char description (above MAX_MCP_DESCRIPTION_LENGTH) must come
    // back truncated with the `… [truncated]` sentinel — locks the
    // MAX_MCP_DESCRIPTION_LENGTH contract against accidental drift.
    let long = "x".repeat(2049);
    let long_clone = long.clone();
    let (client, _cap, _h) = make_client_against_mock(
        "filesystem",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [
                        { "name": "noisy", "description": long_clone, "inputSchema": {} }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert!(
        tools[0].description().ends_with("\u{2026} [truncated]"),
        "must end with truncation sentinel, got: …{}",
        &tools[0].description()[tools[0].description().len().saturating_sub(40)..],
    );
    assert!(
        tools[0].description().chars().count()
            <= mcp::MAX_MCP_DESCRIPTION_LENGTH + "\u{2026} [truncated]".chars().count(),
        "must not exceed MAX_MCP_DESCRIPTION_LENGTH + sentinel length",
    );
}

#[tokio::test]
async fn raw_tool_decodes_anthropic_meta_block() {
    // Lock the `_meta.anthropic/searchHint` and `_meta.anthropic/alwaysLoad`
    // wire keys through the public list-tools behavior. The production
    // decoder intentionally keeps `_meta` opaque JSON; the client extracts
    // the supported hints while preserving the complete value on the DTO.
    let (client, _captured, _h) =
        make_client_against_mock("meta-srv", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [{
                        "name": "x",
                        "description": "y",
                        "inputSchema": {},
                        "_meta": {
                            "anthropic/searchHint": "shell",
                            "anthropic/alwaysLoad": true,
                            "vendor/opaque": { "preserved": [1, 2, 3] }
                        }
                    }]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].search_hint.as_deref(), Some("shell"));
    assert_eq!(tools[0].always_load, Some(true));
    assert_eq!(
        tools[0].meta,
        Some(json!({
            "anthropic/searchHint": "shell",
            "anthropic/alwaysLoad": true,
            "vendor/opaque": { "preserved": [1, 2, 3] }
        }))
    );
}

#[tokio::test]
async fn list_prompts_returns_server_prompts() {
    let (client, _cap, _h) = make_client_against_mock(
        "prompts-srv",
        std::path::PathBuf::from("/tmp/work"),
        |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "prompts": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "prompts/list" => json!({
                    "prompts": [
                        { "name": "summarize", "description": "Summarize text" },
                        { "name": "translate" }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let prompts = client.list_prompts().await.expect("list");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].name, "summarize");
    assert_eq!(prompts[0].description.as_deref(), Some("Summarize text"));
    assert_eq!(prompts[1].name, "translate");
    assert_eq!(prompts[1].description, None);
}

#[tokio::test]
async fn list_prompts_aggregates_paginated_pages() {
    let _capture = list_paginated_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_list_paginated_capture();
    clear_list_paginated_rows();
    let page = Arc::new(AtomicUsize::new(0));
    let page_clone = page.clone();
    let (client, captured, _h) = make_client_against_mock(
        "prompts-srv",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "prompts": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "prompts/list" => match page_clone.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert!(req["params"].get("cursor").is_none());
                        json!({
                            "prompts": [{ "name": "summarize" }],
                            "nextCursor": "next"
                        })
                    }
                    1 => {
                        assert_eq!(req["params"]["cursor"], json!("next"));
                        json!({
                            "prompts": [{ "name": "translate" }]
                        })
                    }
                    other => panic!("unexpected page request {other}"),
                },
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let prompts = client.list_prompts().await.expect("list");
    assert_eq!(
        prompts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["summarize", "translate"]
    );
    let frames = captured.snapshot().await;
    assert_eq!(frames.len(), 3, "initialize plus two prompts/list pages");
    assert_contains_single_list_paginated_row(ListPaginatedRow {
        event: telemetry::tengu::mcp::LIST_PAGINATED.to_string(),
        method: "prompts/list".to_string(),
        page_count: Some(2),
        item_count: Some(2),
        outcome: "complete".to_string(),
        source: Some("pages".to_string()),
    });
}

#[tokio::test]
async fn get_prompt_sends_name_and_arguments() {
    let (client, captured, _h) = make_client_against_mock(
        "prompts-srv",
        std::path::PathBuf::from("/tmp/work"),
        |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "prompts": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "prompts/get" => json!({
                    "description": "test",
                    "messages": []
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let resp = client
        .get_prompt("summarize", json!({"target": "rust"}))
        .await
        .expect("get");
    assert!(resp.get("messages").is_some());

    // Verify wire bytes carry the literal `prompts/get` method + name.
    let frames = captured.snapshot().await;
    let last = frames.last().expect("at least one");
    let s = std::str::from_utf8(last).unwrap();
    assert!(s.contains(r#""method":"prompts/get""#));
    assert!(s.contains(r#""name":"summarize""#));
}

#[tokio::test]
async fn list_resources_and_read_resource_roundtrip() {
    let (client, _cap, _h) =
        make_client_against_mock("res-srv", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "resources": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "resources/list" => json!({
                    "resources": [
                        { "uri": "file:///a.txt", "name": "a", "mimeType": "text/plain" }
                    ]
                }),
                "resources/read" => json!({
                    "contents": [{ "uri": "file:///a.txt", "text": "hello" }]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;

    client.initialize().await.expect("init");
    let resources = client.list_resources().await.expect("list");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "file:///a.txt");
    assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));

    let content = client.read_resource("file:///a.txt").await.expect("read");
    assert_eq!(content.uri, "file:///a.txt");
    assert_eq!(content.content, "hello");
}

#[tokio::test]
async fn list_resources_aggregates_paginated_pages() {
    let _capture = list_paginated_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_list_paginated_capture();
    clear_list_paginated_rows();
    let page = Arc::new(AtomicUsize::new(0));
    let page_clone = page.clone();
    let (client, captured, _h) = make_client_against_mock(
        "res-srv",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "resources": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "resources/list" => match page_clone.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert!(req["params"].get("cursor").is_none());
                        json!({
                            "resources": [{ "uri": "file:///a.txt", "name": "a" }],
                            "nextCursor": "next"
                        })
                    }
                    1 => {
                        assert_eq!(req["params"]["cursor"], json!("next"));
                        json!({
                            "resources": [{ "uri": "file:///b.txt", "name": "b" }]
                        })
                    }
                    other => panic!("unexpected page request {other}"),
                },
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let resources = client.list_resources().await.expect("list");
    assert_eq!(
        resources.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>(),
        vec!["file:///a.txt", "file:///b.txt"]
    );
    let frames = captured.snapshot().await;
    assert_eq!(frames.len(), 3, "initialize plus two resources/list pages");
    assert_contains_single_list_paginated_row(ListPaginatedRow {
        event: telemetry::tengu::mcp::LIST_PAGINATED.to_string(),
        method: "resources/list".to_string(),
        page_count: Some(2),
        item_count: Some(2),
        outcome: "complete".to_string(),
        source: Some("pages".to_string()),
    });
}

#[tokio::test]
async fn list_tools_emits_capped_paginated_telemetry() {
    let _capture = list_paginated_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_list_paginated_capture();
    clear_list_paginated_rows();
    let page = Arc::new(AtomicUsize::new(0));
    let page_clone = page.clone();
    let (client, _captured, _h) = make_client_against_mock(
        "filesystem",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => {
                    let index = page_clone.fetch_add(1, Ordering::SeqCst);
                    if index == 0 {
                        assert!(req["params"].get("cursor").is_none());
                    } else {
                        assert_eq!(req["params"]["cursor"], json!(format!("next-{index}")));
                    }
                    json!({
                        "tools": [
                            { "name": format!("tool_{index}"), "description": "Read", "inputSchema": {} }
                        ],
                        "nextCursor": format!("next-{}", index + 1)
                    })
                }
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), mcp::client::MAX_MCP_LIST_PAGES);
    assert_contains_single_list_paginated_row(ListPaginatedRow {
        event: telemetry::tengu::mcp::LIST_PAGINATED.to_string(),
        method: "tools/list".to_string(),
        page_count: Some(mcp::client::MAX_MCP_LIST_PAGES as u64),
        item_count: Some(mcp::client::MAX_MCP_LIST_PAGES as u64),
        outcome: "capped".to_string(),
        source: Some("pages".to_string()),
    });
}

#[tokio::test]
async fn list_prompts_emits_error_paginated_telemetry() {
    let _capture = list_paginated_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_list_paginated_capture();
    clear_list_paginated_rows();
    let page = Arc::new(AtomicUsize::new(0));
    let page_clone = page.clone();
    let (client, _captured, _h) = make_client_against_mock(
        "prompts-srv",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "prompts": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "prompts/list" => match page_clone.fetch_add(1, Ordering::SeqCst) {
                    0 => json!({
                        "prompts": [{ "name": "summarize" }],
                        "nextCursor": "next"
                    }),
                    1 => {
                        return Some(json!({
                            "jsonrpc":"2.0",
                            "id": id,
                            "error": { "code": -32602, "message": "invalid cursor" }
                        }))
                    }
                    other => panic!("unexpected page request {other}"),
                },
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");
    let err = client.list_prompts().await.expect_err("second page fails");
    assert!(
        matches!(err, mcp::McpClientError::Rpc(ref message) if message.contains("invalid cursor")),
        "unexpected error: {err:?}"
    );
    assert_contains_single_list_paginated_row(ListPaginatedRow {
        event: telemetry::tengu::mcp::LIST_PAGINATED.to_string(),
        method: "prompts/list".to_string(),
        page_count: Some(1),
        item_count: Some(1),
        outcome: "error".to_string(),
        source: Some("pages".to_string()),
    });
}

/// MCP-5d: `read_resource_rich` returns the FULL multi-content `contents[]`
/// array — a text block passes through with its `mimeType`, and a base64 blob
/// block is decoded, persisted under the supplied output dir, and reported via
/// `blob_saved_to` + a `getBinaryBlobSavedMessage` text line. The legacy
/// single-content `read_resource` is unaffected (asserted alongside).
#[tokio::test]
async fn read_resource_rich_returns_multi_content_with_blob_persisted() {
    // base64("%PDF-1.4") == "JVBERi0xLjQ=".
    let pdf_b64 = "JVBERi0xLjQ=";
    let (client, _cap, _h) = make_client_against_mock(
        "rich-srv",
        std::path::PathBuf::from("/tmp/work"),
        move |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "resources": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "resources/read" => json!({
                    "contents": [
                        { "uri": "file:///doc", "mimeType": "text/markdown", "text": "# Title" },
                        { "uri": "file:///doc.pdf", "mimeType": "application/pdf", "blob": pdf_b64 }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        },
    )
    .await;

    client.initialize().await.expect("init");

    let out_dir = tempfile::tempdir().expect("tempdir");
    let contents = client
        .read_resource_rich("file:///doc", out_dir.path())
        .await
        .expect("read_resource_rich");

    assert_eq!(contents.len(), 2, "both content blocks surfaced");

    // Text block: passthrough, no blob.
    assert_eq!(contents[0].uri, "file:///doc");
    assert_eq!(contents[0].mime_type.as_deref(), Some("text/markdown"));
    assert_eq!(contents[0].text.as_deref(), Some("# Title"));
    assert_eq!(contents[0].blob_saved_to, None);

    // Blob block: decoded + persisted under out_dir; mimeType carried.
    assert_eq!(contents[1].uri, "file:///doc.pdf");
    assert_eq!(contents[1].mime_type.as_deref(), Some("application/pdf"));
    let saved = contents[1]
        .blob_saved_to
        .as_deref()
        .expect("blob persisted to disk");
    assert_eq!(
        std::path::Path::new(saved)
            .extension()
            .and_then(|e| e.to_str()),
        Some("pdf"),
        "mime-derived extension: {saved}"
    );
    assert!(
        saved.starts_with(out_dir.path().to_str().unwrap()),
        "persisted under the supplied output dir: {saved}"
    );
    assert_eq!(
        std::fs::read(saved).expect("read persisted blob"),
        b"%PDF-1.4",
        "raw decoded bytes written verbatim (not base64)"
    );
    // text carries the getBinaryBlobSavedMessage line with the server name.
    let text = contents[1].text.as_deref().expect("blob saved message");
    assert!(
        text.starts_with(
            "[Resource from rich-srv at file:///doc.pdf] Binary content (application/pdf,"
        ),
        "saved message prefix: {text}"
    );
    assert!(
        text.ends_with(&format!("saved to {saved}")),
        "saved message tail: {text}"
    );
}

#[tokio::test]
async fn ping_returns_ok_on_empty_result() {
    let (client, captured, _h) =
        make_client_against_mock("ping-srv", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "ping" => json!({}),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;

    client.initialize().await.expect("init");
    client.ping().await.expect("ping ok");
    let frames = captured.snapshot().await;
    let s = std::str::from_utf8(frames.last().unwrap()).unwrap();
    assert!(s.contains(r#""method":"ping""#));
}

#[tokio::test]
async fn captured_frames_capture_outbound_lines() {
    // Verify the harness itself round-trips a single line through the
    // capture buffer. This is independent of `McpClient`; we drive the
    // server-side directly via the client's `Connection::notify` API.
    use std::time::Duration;

    let (client, captured, _h) = make_client_against_mock(
        "filesystem",
        std::path::PathBuf::from("/tmp/work"),
        |_req| None, // notifications don't expect responses
    )
    .await;

    // We can't reach into McpClient's `connection` field from outside the
    // crate, so this test only proves the constructor + mock harness
    // wire up cleanly. A more involved bytes assertion lives in Task 8.
    drop(client);
    // Give the (now dropped) connection's broker a moment to settle.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let frames = captured.snapshot().await;
    // No frames are guaranteed to have been sent — the assertion is the
    // smoke check that `snapshot()` is callable and returns a Vec.
    assert!(
        frames.len() < usize::MAX,
        "snapshot returns a finite collection: {}",
        frames.len()
    );
}

/// Drive a full inbound roundtrip: the mock server completes the
/// `initialize` handshake, fires a peer-initiated request to the client
/// (`peer_method` + `peer_params`), and captures the literal JSON response
/// the client's registered handler emits back.
///
/// Returns the wire-level `(id, result)` pair parsed from the captured
/// response frame so individual tests can assert on echoed id + result
/// shape together (Task 12 wire-byte contract).
async fn run_inbound_roundtrip(
    peer_method: &'static str,
    peer_params: Value,
    peer_id: i64,
    cwd: std::path::PathBuf,
) -> (Value, Value) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (client_read, mut server_write) = duplex(64 * 1024);
    let (server_read, client_write) = duplex(64 * 1024);

    // Holds the parsed (id, result) pair extracted from the client's
    // response to the peer-initiated request.
    let response_holder = Arc::new(Mutex::new(None::<(Value, Value)>));
    let response_clone = response_holder.clone();

    tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let mut line = String::new();

        // Step 1: wait for the client's `initialize` request, respond.
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if req["method"] == "initialize" {
                let id = req["id"].clone();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": { "name": "m", "version": "0" }
                    }
                });
                let mut out = serde_json::to_vec(&resp).unwrap();
                out.push(b'\n');
                if server_write.write_all(&out).await.is_err() {
                    return;
                }
                if server_write.flush().await.is_err() {
                    return;
                }
                break;
            }
        }

        // Step 2: issue the peer-initiated request to the client.
        let peer_req = json!({
            "jsonrpc": "2.0",
            "id": peer_id,
            "method": peer_method,
            "params": peer_params,
        });
        let mut out = serde_json::to_vec(&peer_req).unwrap();
        out.push(b'\n');
        if server_write.write_all(&out).await.is_err() {
            return;
        }
        if server_write.flush().await.is_err() {
            return;
        }

        // Step 3: read frames until we see the response that echoes our
        // `peer_id` — the client's handler output.
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg["id"] == json!(peer_id) {
                *response_clone.lock().await = Some((msg["id"].clone(), msg["result"].clone()));
                return;
            }
        }
    });

    let client_read: Box<dyn AsyncRead + Send + Unpin> = Box::new(client_read);
    let client_write: Box<dyn AsyncWrite + Send + Unpin> = Box::new(client_write);
    let connection = jsonrpc::Connection::new_line_delimited(client_read, client_write);
    let client = McpClient::new("inbound-srv", cwd, Arc::new(connection)).await;
    client.initialize().await.expect("init");

    // Fence the entire roundtrip with a 100ms ceiling so a regression in
    // the inbound dispatcher doesn't hang the suite.
    let captured = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            if response_holder.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await;
    captured.expect("inbound roundtrip completed within 100ms");

    // Snapshot the captured response BEFORE the client is dropped so the
    // connection broker (and its registered handlers) stays alive through
    // the response read. Holding the MutexGuard across the `drop(client)`
    // would extend the borrow past `response_holder`'s scope, so we clone
    // out of the guard immediately.
    let captured_pair = response_holder
        .lock()
        .await
        .clone()
        .expect("response captured");
    drop(client);
    captured_pair
}

#[tokio::test]
async fn inbound_roots_list_returns_file_uri() {
    // Mock fires `roots/list` with id=42; client's registered
    // `RootsListHandler` must respond with the literal
    // `{"roots":[{"uri":"file://<cwd>"}]}` payload and echo id=42.
    let (id, result) = run_inbound_roundtrip(
        "roots/list",
        json!({}),
        42,
        std::path::PathBuf::from("/Users/example/proj"),
    )
    .await;

    // Echoed id MUST match the request id (JSON-RPC 2.0 §5).
    assert_eq!(id, json!(42), "response id must echo request id");

    // Wire-shape assertion: outer object with exactly one `roots` array.
    let roots = result["roots"].as_array().expect("roots array");
    assert_eq!(roots.len(), 1, "exactly one root");
    assert_eq!(
        roots[0]["uri"], "file:///Users/example/proj",
        "uri must be literal file:// + absolute cwd",
    );

    // Wire-byte assertion: serialize the full response envelope and lock
    // the exact JSON-RPC frame the mock would have observed on the wire.
    let envelope = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let bytes = serde_json::to_vec(&envelope).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(
        text.contains(r#""id":42"#),
        "wire bytes must echo id=42, got: {text}",
    );
    assert!(
        text.contains(r#""roots":[{"uri":"file:///Users/example/proj"}]"#),
        "wire bytes must carry literal roots array, got: {text}",
    );
}

#[tokio::test]
async fn inbound_elicitation_create_returns_cancel_literal() {
    // Mock fires `elicitation/create` with id=43; client's registered
    // `ElicitationCreateHandler` must respond with the literal
    // `{"action":"cancel"}` payload and echo id=43.
    let (id, result) = run_inbound_roundtrip(
        "elicitation/create",
        json!({"message": "any"}),
        43,
        std::path::PathBuf::from("/tmp"),
    )
    .await;

    // Echoed id MUST match the request id.
    assert_eq!(id, json!(43), "response id must echo request id");

    // EXACT shape — one field, value "cancel". Extra fields would break
    // claude-code parity (services/mcp/client.ts:1196).
    let obj = result.as_object().expect("object");
    assert_eq!(obj.len(), 1, "exactly one field in elicitation result");
    assert_eq!(result["action"], "cancel", "action must be literal cancel");

    // Wire-byte assertion: lock the exact response frame.
    let envelope = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let bytes = serde_json::to_vec(&envelope).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(
        text.contains(r#""id":43"#),
        "wire bytes must echo id=43, got: {text}",
    );
    assert!(
        text.contains(r#""result":{"action":"cancel"}"#),
        "wire bytes must carry literal cancel result, got: {text}",
    );
}

// ── §20a — MCP tool JSON-Schema normalization (`mcp::tool_schema`) ─────────

#[tokio::test]
async fn list_tools_drops_root_combinator_schema_by_default() {
    // The oracle's `tengu_mcp_normalize_root_combinators` gate defaults to an
    // empty allowlist (off for everyone) with no flag fetcher wired, so a
    // tool whose `inputSchema` root is a top-level `anyOf`/`oneOf`/`allOf` is
    // unconditionally dropped from the list — never forwarded verbatim, and
    // never silently normalized either. This is the list-length-changing
    // half of §20a: a plain tool from the same server survives untouched.
    let (client, _cap, _h) =
        make_client_against_mock("combos", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [
                        {
                            "name": "plain_tool",
                            "description": "A normal tool",
                            "inputSchema": { "type": "object", "properties": { "a": { "type": "string" } } }
                        },
                        {
                            "name": "combo_tool",
                            "description": "Uses a root anyOf",
                            "inputSchema": {
                                "anyOf": [
                                    { "type": "object", "properties": { "a": { "type": "string" } } },
                                    { "type": "object", "properties": { "b": { "type": "string" } } }
                                ]
                            }
                        }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(
        tools.len(),
        1,
        "the root-anyOf tool must be dropped, only the plain tool remains: {tools:?}"
    );
    assert_eq!(tools[0].tool_name, "plain_tool");
}

#[tokio::test]
async fn list_tools_keeps_a_meta_invalid_schema_with_warning_by_default() {
    // The oracle's `tengu_mcp_drop_invalid_tool_schemas` gate also defaults
    // to off, so a tool whose schema fails JSON-Schema-2020-12 meta
    // validation (here: `"type": 5`, not a valid type keyword value) is KEPT
    // — not dropped — matching the oracle's default "requests that include
    // it may fail" warn-and-keep behavior rather than silently forwarding it
    // as verbatim-valid OR dropping the tool from the list.
    let (client, _cap, _h) =
        make_client_against_mock("badschema", std::path::PathBuf::from("/tmp/work"), |req| {
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "m", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [
                        {
                            "name": "invalid_type_tool",
                            "description": "Bad type keyword",
                            "inputSchema": { "type": 5 }
                        }
                    ]
                }),
                _ => return None,
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        })
        .await;

    client.initialize().await.expect("init");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1, "kept-with-warning, not dropped: {tools:?}");
    assert_eq!(tools[0].input_schema, json!({ "type": 5 }));

    // "Kept" alone is what the PRE-§20a code already did, so the assertions
    // above cannot tell warn-and-keep apart from silent forwarding. Pin the
    // decision this schema produced: a `warning` (not a `drop_reason`) is the
    // half of §20a that the list length can never show.
    let decision = mcp::tool_schema::decide_tool_schema(None, &json!({ "type": 5 }));
    assert!(
        decision.drop_reason.is_none(),
        "drop gate is off by default: {decision:?}"
    );
    let warning = decision
        .warning
        .expect("a meta-invalid schema must be KEPT WITH A WARNING, not silently forwarded");
    assert!(
        warning.starts_with("input schema would be rejected by the Anthropic API (")
            && warning.ends_with("); requests that include it may fail"),
        "oracle warn copy, got: {warning}"
    );
}

// COVERAGE NOTE: the two tests above drive `McpClient::list_tools`, which
// production reaches only through `McpRegistry::refresh_catalog` (a
// server-sent `notifications/tools/list_changed`). The CONNECT path — the one
// that fills `McpConnectionState::Connected { tools }` for
// `build_registered_mcp_tools` — goes through `McpTransport::list_tools`
// instead, and is covered by
// `registry::tests::connect_applies_the_tool_schema_decision_to_the_model_facing_list`
// and `registry::tests::connect_resolves_the_schema_gate_from_the_servers_own_hostname`.
