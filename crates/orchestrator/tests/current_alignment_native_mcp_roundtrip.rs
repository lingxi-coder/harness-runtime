//! Replay the pinned Native 2.1.293 MCP structured-content result through the
//! current MCP client/tool, Orchestrator, ApiService, SDK request codec, and
//! session JSONL writer.
//!
//! The MCP peer and Anthropic responses are fixture-backed and local. This
//! test observes the request bytes sent by the real SDK transport and checks
//! the independently captured Native model-tool-result and JSONL artifacts.

mod common;

use bytes::Bytes;
use common::{
    api_service, assert_fixed_usage, paired_mcp_connection, read_fixture, FixedAnthropicTransport,
    UnusedMcpTransport, MODEL,
};
use lingxi_core::host::{FileSystem, OutputEvent};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{McpConnectionId, SessionId};
use mcp::{McpClient, McpRegistry};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::Tool;
use tool_mcp::MCPTool;

fn find_tool_result(
    request: &Utf16JsonProjection,
    tool_use_id: &str,
) -> Option<Utf16JsonProjection> {
    let messages = request.array_len("/messages")?;
    for message_index in 0..messages {
        let content = format!("/messages/{message_index}/content");
        let Some(blocks) = request.array_len(&content) else {
            continue;
        };
        for block_index in 0..blocks {
            let base = format!("{content}/{block_index}");
            if request.string_at(&format!("{base}/type")) == Some("tool_result")
                && request.string_at(&format!("{base}/tool_use_id")) == Some(tool_use_id)
            {
                return request.subprojection(&base).ok();
            }
        }
    }
    None
}
const TOOL_USE_ID: &str = "toolu_native_mcp_fixture_1";
const SESSION_UUID: &str = "00000000-0000-4000-8000-000000002943";
const FIXTURE_DIR: &str = "../../docs/parity/current-alignment/fixtures/mcp-roundtrip/native-baseline/structured-content-lone";

async fn mcp_peer(
    fixture_dir: PathBuf,
    response_tx: tokio::sync::mpsc::Sender<Bytes>,
    mut request_rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> Vec<Vec<u8>> {
    let native_responses = std::fs::read_to_string(fixture_dir.join("mcp-stdio-out.raw.jsonl"))
        .expect("captured Native MCP stdio output");
    // The captured `tools/call` response contains an escaped lone UTF-16
    // surrogate. `serde_json::Value` cannot materialize that response string,
    // so parse only the numeric JSON-RPC id through the source-preserving
    // projection and retain the original response line unchanged.
    let response_by_native_id = native_responses
        .lines()
        .filter_map(|line| {
            let value = Utf16JsonProjection::parse(line).ok()?;
            let id = value.number_at("/id")?;
            (id >= 0.0 && id.fract() == 0.0).then(|| (id as i64, line.to_owned()))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let expected_methods = ["initialize", "tools/list", "tools/call"];
    let mut captured = Vec::with_capacity(expected_methods.len());

    for (native_id, method) in expected_methods.into_iter().enumerate() {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv())
            .await
            .expect("MCP client request arrives")
            .expect("MCP connection remains open");
        let request: Value = serde_json::from_slice(&frame).expect("MCP request JSON");
        assert_eq!(request.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
        assert_eq!(request.get("method").and_then(Value::as_str), Some(method));
        let request_id = request.get("id").expect("request has an id").to_string();
        let native_response = response_by_native_id
            .get(&(native_id as i64))
            .unwrap_or_else(|| panic!("Native stdio fixture has a response for id {native_id}"));
        let native_id_field = format!("\"id\":{native_id}");
        let request_id_field = format!("\"id\":{request_id}");
        let response = native_response.replacen(&native_id_field, &request_id_field, 1);
        response_tx
            .send(Bytes::from(format!("{response}\n")))
            .await
            .expect("deliver captured Native MCP result");
        captured.push(frame.to_vec());
    }
    captured
}

fn fixed_session_id() -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(SESSION_UUID).expect("fixed Native session ID"))
}

#[tokio::test]
async fn native_mcp_structured_content_round_trips_through_orchestrator_and_sdk() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    let native_stdio_in = std::fs::read_to_string(fixture_dir.join("mcp-stdio-in.raw.jsonl"))
        .expect("captured Native MCP stdio input");
    let native_stdio_out = std::fs::read_to_string(fixture_dir.join("mcp-stdio-out.raw.jsonl"))
        .expect("captured Native MCP stdio output");
    assert!(native_stdio_in.contains("\"method\":\"tools/list\""));
    assert!(native_stdio_in.contains("\"method\":\"tools/call\""));
    assert!(native_stdio_out.contains("\"structuredContent\""));

    let native_tool_result = Utf16JsonProjection::parse(
        &std::fs::read_to_string(fixture_dir.join("model-tool-result.raw.json"))
            .expect("captured Native model tool result"),
    )
    .expect("Native tool-result JSON");
    let expected_model_content = native_tool_result
        .subprojection("/content")
        .expect("Native model content projection");
    let native_stdout: Value =
        serde_json::from_slice(&read_fixture(&fixture_dir, "native.stdout.raw.json"))
            .expect("captured Native stdout envelope");
    assert_eq!(native_stdout["result"], "MCP fixture completed.");
    assert_eq!(native_stdout["num_turns"], 2);

    assert_fixed_usage(&fixture_dir, "model-response-tool-use.raw.sse");
    assert_fixed_usage(&fixture_dir, "model-response-final.raw.sse");

    let (mcp_connection, mcp_response_tx, mcp_request_rx) = paired_mcp_connection();
    let mcp_peer_task = tokio::spawn(mcp_peer(
        fixture_dir.clone(),
        mcp_response_tx,
        mcp_request_rx,
    ));
    let mcp_client =
        Arc::new(McpClient::new("fixture", std::env::temp_dir(), mcp_connection).await);
    mcp_client
        .initialize()
        .await
        .expect("MCP initialize response");
    let discovered_tools = mcp_client
        .list_tools()
        .await
        .expect("MCP tools/list response");
    assert_eq!(
        discovered_tools.len(),
        1,
        "Native lists the fixture tool once"
    );
    let discovered = discovered_tools.into_iter().next().expect("one MCP tool");
    assert_eq!(discovered.full_name, "mcp__fixture__emit_payload");
    let native_tools: Utf16JsonProjection = Utf16JsonProjection::parse(
        &std::fs::read_to_string(fixture_dir.join("native-tools-slice.raw.json"))
            .expect("Native advertised MCP tool slice"),
    )
    .expect("Native tool slice JSON");
    assert_eq!(
        discovered.description.string_units(""),
        native_tools.string_units("/0/description")
    );
    let native_schema = native_tools
        .subprojection("/0/input_schema")
        .expect("Native tool schema projection");
    assert_eq!(discovered.input_schema.string_at("/type"), Some("object"));
    assert_eq!(
        discovered.input_schema.string_at("/properties/mode/type"),
        native_schema.string_at("/properties/mode/type")
    );
    assert_eq!(
        discovered.input_schema.string_at("/properties/mode/enum/0"),
        native_schema.string_at("/properties/mode/enum/0"),
        "the actual MCPClient discovery supplies the model-facing schema"
    );

    let mcp_registry = Arc::new(McpRegistry::new(Arc::new(UnusedMcpTransport)));
    mcp_registry.register_client("fixture", mcp_client).await;
    let temp = tempdir().expect("isolated Orchestrator session root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create isolated cwd");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let session_path = temp.path().join("session.jsonl");
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    let mut tool_context = tool_api::test_support::ctx_for_file_tools(
        fs.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![workspace.clone()],
    );
    tool_context.mcp_registry = Some(mcp_registry.clone());
    let mcp_tool = Arc::new(MCPTool::new_for_tool(
        tool_context,
        discovered.full_name,
        discovered.description,
        discovered.input_schema,
        discovered.output_schema,
        None,
        discovered.search_hint,
        discovered.always_load.unwrap_or(false),
        discovered.requires_user_interaction,
    )) as Arc<dyn Tool>;
    let tools = Arc::new(ToolRegistry::new());
    tools.register_mcp_tools(McpConnectionId::new(), vec![mcp_tool]);

    let sse_transport = Arc::new(FixedAnthropicTransport {
        responses: Mutex::new(VecDeque::from([
            read_fixture(&fixture_dir, "model-response-tool-use.raw.sse"),
            read_fixture(&fixture_dir, "model-response-final.raw.sse"),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(sse_transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 2,
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            workspace.clone(),
        )
        .with_session_id(fixed_session_id())
        .with_jsonl_writer(writer)
        .with_mcp_registry(mcp_registry),
    );

    let outcome = orchestrator
        .run_turn_streaming(
            "Use the only available MCP tool once, then finish with the fixed final response.",
        )
        .await
        .expect("fixed Native-shaped MCP roundtrip");
    assert!(matches!(
        outcome,
        ConversationOutcome::EndTurn { turn_count: 2, .. }
    ));

    let captured_mcp_requests = mcp_peer_task.await.expect("MCP fixture peer");
    assert_eq!(captured_mcp_requests.len(), 3);
    let mcp_methods = captured_mcp_requests
        .iter()
        .map(|raw| {
            let request: Value = serde_json::from_slice(raw).expect("captured MCP request JSON");
            request["method"].as_str().expect("MCP method").to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(mcp_methods, ["initialize", "tools/list", "tools/call"]);
    let native_mcp_call_request = Utf16JsonProjection::parse(
        std::str::from_utf8(&read_fixture(
            &fixture_dir,
            "mcp-tools-call-request.raw.jsonrpc",
        ))
        .expect("Native MCP tools/call request is UTF-8"),
    )
    .expect("Native MCP tools/call request source");
    let captured_call: Value =
        serde_json::from_slice(&captured_mcp_requests[2]).expect("captured tools/call request");
    assert_eq!(captured_call["method"], "tools/call");
    assert_eq!(captured_call["params"]["name"], "emit_payload");
    assert_eq!(
        captured_call["params"]["arguments"],
        json!({"mode":"source-units"})
    );
    assert_eq!(
        captured_call["params"]["_meta"]["claudecode/toolUseId"],
        TOOL_USE_ID
    );
    assert_eq!(
        native_mcp_call_request.string_at("/params/name"),
        Some("emit_payload")
    );

    let sdk_requests = sse_transport
        .requests
        .lock()
        .expect("captured SDK request lock")
        .clone();
    assert_eq!(
        sdk_requests.len(),
        2,
        "one SDK request per Native SSE fixture"
    );
    assert!(sse_transport
        .responses
        .lock()
        .expect("fixed Native SSE queue lock")
        .is_empty());
    let sdk_request = |index: usize| {
        Utf16JsonProjection::parse(
            std::str::from_utf8(&sdk_requests[index]).expect("SDK body is UTF-8 JSON"),
        )
        .expect("parse captured SDK source without reserialization")
    };
    let initial = sdk_request(0);
    let followup = sdk_request(1);
    for request in [&initial, &followup] {
        assert_eq!(request.string_at("/model"), Some(MODEL));
        assert_eq!(
            request
                .at_pointer("/stream")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }
    let initial_tools = initial.array_len("/tools").expect("SDK tool definitions");
    assert!(
        (0..initial_tools).any(|index| {
            initial.string_at(&format!("/tools/{index}/name")) == Some("mcp__fixture__emit_payload")
        }),
        "the discovered MCP tool reaches the actual SDK request"
    );
    let model_tool_result = find_tool_result(&followup, TOOL_USE_ID)
        .expect("follow-up SDK request contains Native tool_result");
    assert_eq!(
        model_tool_result.string_units("/content"),
        expected_model_content.string_units(""),
        "full structuredContent JSON text and fixed total-token reminder reach SDK body"
    );
    assert_eq!(
        model_tool_result.string_at("/tool_use_id"),
        Some(TOOL_USE_ID)
    );
    assert!(model_tool_result.at_pointer("/is_error").is_none());
    assert_eq!(
        expected_model_content.string_at(""),
        Some(
            "{\"ok\":true,\"label\":\"structured-\\ud800Ω\"}\n\n<system-reminder>\n<total_tokens>14999985 tokens left</total_tokens>\n</system-reminder>"
        ),
        "Native's structuredContent is JSON text before the inline reminder"
    );

    let output_events = output.snapshot().await;
    let final_text = output_events.iter().rev().find_map(|event| match event {
        OutputEvent::Text { text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(
        final_text.as_ref().and_then(|text| text.string_at("")),
        native_stdout["result"].as_str(),
        "actual Orchestrator output matches Native stdout result"
    );

    let raw_jsonl = std::fs::read_to_string(&session_path).expect("captured Orchestrator JSONL");
    let candidate_tool_use = raw_jsonl
        .lines()
        .filter_map(|line| Utf16JsonProjection::parse(line).ok())
        .find(|row| {
            row.string_at("/type") == Some("assistant")
                && row.string_at("/message/content/0/type") == Some("tool_use")
                && row.string_at("/message/content/0/id") == Some(TOOL_USE_ID)
        })
        .expect("persisted assistant MCP tool-use row");
    let candidate_tool_result = raw_jsonl
        .lines()
        .filter_map(|line| Utf16JsonProjection::parse(line).ok())
        .find(|row| {
            row.string_at("/type") == Some("user")
                && row.string_at("/message/content/0/type") == Some("tool_result")
                && row.string_at("/message/content/0/tool_use_id") == Some(TOOL_USE_ID)
        })
        .expect("persisted user MCP tool-result row");
    let native_rows_raw =
        std::fs::read_to_string(fixture_dir.join("tool-related-session-rows.raw.jsonl"))
            .expect("captured Native tool-related JSONL rows");
    let native_rows = native_rows_raw
        .lines()
        .map(|line| Utf16JsonProjection::parse(line).expect("Native JSONL row"))
        .collect::<Vec<_>>();
    let native_tool_use = native_rows
        .iter()
        .find(|row| row.string_at("/type") == Some("assistant"))
        .expect("Native assistant tool-use row");
    let native_tool_result_row = native_rows
        .iter()
        .find(|row| row.string_at("/type") == Some("user"))
        .expect("Native user tool-result row");
    assert_eq!(
        candidate_tool_use.string_at("/message/content/0/name"),
        native_tool_use.string_at("/message/content/0/name")
    );
    assert_eq!(
        candidate_tool_use.string_at("/message/content/0/id"),
        native_tool_use.string_at("/message/content/0/id")
    );
    assert_eq!(
        candidate_tool_use.string_at("/message/content/0/input/mode"),
        native_tool_use.string_at("/message/content/0/input/mode")
    );
    assert_eq!(
        candidate_tool_result.string_units("/message/content/0/content"),
        native_tool_result_row.string_units("/message/content/0/content"),
        "JSONL stores the bare Native tool result, without the request-only reminder"
    );
    assert_eq!(
        candidate_tool_result.string_units("/toolUseResult"),
        native_tool_result_row.string_units("/toolUseResult")
    );
    assert_eq!(
        candidate_tool_result.string_units("/mcpMeta/structuredContent/label"),
        Some(
            "structured-"
                .encode_utf16()
                .chain([0xd800, 0x03a9])
                .collect()
        ),
        "the JSONL MCP metadata retains structuredContent's lone UTF-16 unit"
    );
    assert_eq!(
        candidate_tool_result.string_at("/sessionId"),
        Some(SESSION_UUID)
    );
    assert_eq!(
        candidate_tool_result.string_at("/message/content/0/tool_use_id"),
        Some(TOOL_USE_ID)
    );
    assert_eq!(candidate_tool_result.string_at("/cwd"), workspace.to_str());

    let typed_rows = JsonlReader::new(session_path, fs)
        .read_all()
        .await
        .expect("read typed persisted rows");
    assert!(typed_rows.iter().any(|row| {
        row.message_type == "user"
            && row.message.string_at("/content/0/tool_use_id") == Some(TOOL_USE_ID)
    }));
}
