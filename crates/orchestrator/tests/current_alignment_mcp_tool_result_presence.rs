//! Compare MCP tool-result presence, model-request bytes, and session persistence with Native 2.1.295 fixtures.

mod common;

use bytes::Bytes;
use common::{
    api_service, assert_fixed_usage, paired_mcp_connection, read_fixture, FixedAnthropicTransport,
    UnusedMcpTransport, MODEL,
};
use lingxi_core::host::FileSystem;
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
use session::jsonl::writer::JsonlWriter;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::Tool;
use tool_mcp::MCPTool;

const MCP_PRESENCE_NATIVE_DIR: &str = "../../docs/parity/current-alignment/fixtures/tool-result-presence-295/native-baseline/2.1.295/mcp-three-iserror-cases";
const MCP_PRESENCE_INITIALIZE_DIR: &str =
    "../../docs/parity/current-alignment/fixtures/mcp-mime-295/native-baseline/2.1.295/mcp-jsonrpc";
const MCP_PRESENCE_SESSION_ID: &str = "00000000-0000-4000-8000-000000002300";

#[derive(Clone, Copy)]
enum RawPathPart<'a> {
    Key(&'a str),
    Index(usize),
}

fn raw_skip_space(bytes: &[u8], mut position: usize) -> usize {
    while position < bytes.len() && matches!(bytes[position], b' ' | b'\t' | b'\r' | b'\n') {
        position += 1;
    }
    position
}

fn raw_string_end(bytes: &[u8], start: usize) -> usize {
    assert_eq!(
        bytes.get(start),
        Some(&b'"'),
        "expected JSON string at {start}"
    );
    let mut position = start + 1;
    while position < bytes.len() {
        match bytes[position] {
            b'"' => return position + 1,
            b'\\' => position += 2,
            _ => position += 1,
        }
    }
    panic!("unterminated JSON string at {start}");
}

fn raw_value_end(bytes: &[u8], start: usize) -> usize {
    let start = raw_skip_space(bytes, start);
    match bytes.get(start).copied() {
        Some(b'"') => raw_string_end(bytes, start),
        Some(b'{') => {
            let mut position = raw_skip_space(bytes, start + 1);
            if bytes.get(position) == Some(&b'}') {
                return position + 1;
            }
            loop {
                let key_end = raw_string_end(bytes, position);
                position = raw_skip_space(bytes, key_end);
                assert_eq!(
                    bytes.get(position),
                    Some(&b':'),
                    "expected JSON object colon"
                );
                position = raw_value_end(bytes, position + 1);
                position = raw_skip_space(bytes, position);
                match bytes.get(position) {
                    Some(b',') => position = raw_skip_space(bytes, position + 1),
                    Some(b'}') => return position + 1,
                    other => panic!("expected JSON object separator, got {other:?}"),
                }
            }
        }
        Some(b'[') => {
            let mut position = raw_skip_space(bytes, start + 1);
            if bytes.get(position) == Some(&b']') {
                return position + 1;
            }
            loop {
                position = raw_value_end(bytes, position);
                position = raw_skip_space(bytes, position);
                match bytes.get(position) {
                    Some(b',') => position = raw_skip_space(bytes, position + 1),
                    Some(b']') => return position + 1,
                    other => panic!("expected JSON array separator, got {other:?}"),
                }
            }
        }
        Some(_) => {
            let mut position = start;
            while position < bytes.len()
                && !matches!(
                    bytes[position],
                    b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'
                )
            {
                position += 1;
            }
            position
        }
        None => panic!("expected JSON value at end of input"),
    }
}

fn raw_member_span(bytes: &[u8], start: usize, wanted: &str) -> (usize, usize) {
    let start = raw_skip_space(bytes, start);
    assert_eq!(
        bytes.get(start),
        Some(&b'{'),
        "expected JSON object at {start}"
    );
    let mut position = raw_skip_space(bytes, start + 1);
    assert!(
        bytes.get(position) != Some(&b'}'),
        "missing JSON object property {wanted}"
    );
    loop {
        let key_start = position;
        let key_end = raw_string_end(bytes, key_start);
        let key: String =
            serde_json::from_slice(&bytes[key_start..key_end]).expect("JSON property name");
        position = raw_skip_space(bytes, key_end);
        assert_eq!(bytes.get(position), Some(&b':'), "expected property colon");
        let value_start = raw_skip_space(bytes, position + 1);
        let value_end = raw_value_end(bytes, value_start);
        if key == wanted {
            return (value_start, value_end);
        }
        position = raw_skip_space(bytes, value_end);
        match bytes.get(position) {
            Some(b',') => position = raw_skip_space(bytes, position + 1),
            Some(b'}') => panic!("missing JSON object property {wanted}"),
            other => panic!("expected JSON object separator, got {other:?}"),
        }
    }
}

fn raw_element_span(bytes: &[u8], start: usize, wanted: usize) -> (usize, usize) {
    let start = raw_skip_space(bytes, start);
    assert_eq!(
        bytes.get(start),
        Some(&b'['),
        "expected JSON array at {start}"
    );
    let mut position = raw_skip_space(bytes, start + 1);
    assert!(
        bytes.get(position) != Some(&b']'),
        "missing JSON array index {wanted}"
    );
    let mut index = 0;
    loop {
        let value_start = position;
        let value_end = raw_value_end(bytes, value_start);
        if index == wanted {
            return (value_start, value_end);
        }
        index += 1;
        position = raw_skip_space(bytes, value_end);
        match bytes.get(position) {
            Some(b',') => position = raw_skip_space(bytes, position + 1),
            Some(b']') => panic!("missing JSON array index {wanted}"),
            other => panic!("expected JSON array separator, got {other:?}"),
        }
    }
}

fn raw_path_span(bytes: &[u8], path: &[RawPathPart<'_>]) -> (usize, usize) {
    let mut start = 0;
    let mut end = bytes.len();
    for part in path {
        (start, end) = match part {
            RawPathPart::Key(key) => raw_member_span(bytes, start, key),
            RawPathPart::Index(index) => raw_element_span(bytes, start, *index),
        };
    }
    (start, end)
}

fn raw_tool_result_by_id(raw_request: &[u8], tool_use_id: &str) -> Option<Vec<u8>> {
    let request = Utf16JsonProjection::parse(std::str::from_utf8(raw_request).ok()?).ok()?;
    let message_count = request.array_len("/messages")?;
    for message_index in 0..message_count {
        let content_path = format!("/messages/{message_index}/content");
        let Some(block_count) = request.array_len(&content_path) else {
            continue;
        };
        for block_index in 0..block_count {
            let block_path = format!("{content_path}/{block_index}");
            if request.string_at(&format!("{block_path}/type")) == Some("tool_result")
                && request.string_at(&format!("{block_path}/tool_use_id")) == Some(tool_use_id)
            {
                let (start, end) = raw_path_span(
                    raw_request,
                    &[
                        RawPathPart::Key("messages"),
                        RawPathPart::Index(message_index),
                        RawPathPart::Key("content"),
                        RawPathPart::Index(block_index),
                    ],
                );
                return Some(raw_request[start..end].to_vec());
            }
        }
    }
    None
}

async fn mcp_peer_iserror_cases(
    native_dir: PathBuf,
    initialize_dir: PathBuf,
    response_tx: tokio::sync::mpsc::Sender<Bytes>,
    mut request_rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> Vec<Vec<u8>> {
    let response_files = [
        (
            "initialize",
            0,
            initialize_dir,
            "initialize-response-01.raw.jsonrpc",
        ),
        (
            "tools/list",
            2,
            native_dir.clone(),
            "tools-list-response.raw.jsonrpc",
        ),
        (
            "tools/call",
            3,
            native_dir.clone(),
            "tools-call-01-result_omitted-response.raw.jsonrpc",
        ),
        (
            "tools/call",
            4,
            native_dir.clone(),
            "tools-call-02-result_false-response.raw.jsonrpc",
        ),
        (
            "tools/call",
            5,
            native_dir,
            "tools-call-03-result_true-response.raw.jsonrpc",
        ),
    ];
    let expected_calls = [
        ("result_omitted", "toolu_native_iserror_omitted_295"),
        ("result_false", "toolu_native_iserror_false_295"),
        ("result_true", "toolu_native_iserror_true_295"),
    ];
    let mut captured = Vec::with_capacity(response_files.len());

    for (index, (expected_method, native_id, fixture_root, file_name)) in
        response_files.into_iter().enumerate()
    {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv())
            .await
            .expect("candidate MCP request arrives")
            .expect("candidate MCP connection remains open");
        let request: Value = serde_json::from_slice(&frame).expect("candidate MCP request JSON");
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], expected_method);
        if expected_method == "tools/call" {
            let (expected_name, expected_tool_use_id) = expected_calls[index - 2];
            assert_eq!(request["params"]["name"], expected_name);
            assert_eq!(
                request["params"]["arguments"],
                json!({"case": expected_name})
            );
            assert_eq!(
                request["params"]["_meta"]["claudecode/toolUseId"],
                expected_tool_use_id
            );
        }
        let candidate_id = request.get("id").expect("MCP request id").to_string();
        let raw = read_fixture(&fixture_root, file_name);
        let native_response =
            String::from_utf8(raw).expect("captured Native MCP response is UTF-8");
        let native_id_field = format!("\"id\":{native_id}");
        let candidate_id_field = format!("\"id\":{candidate_id}");
        assert!(
            native_response.contains(&native_id_field),
            "Native response fixture id {native_id}"
        );
        let response = native_response.replacen(&native_id_field, &candidate_id_field, 1);
        response_tx
            .send(Bytes::from(format!("{response}\n")))
            .await
            .expect("deliver captured Native MCP result shape");
        captured.push(frame.to_vec());
    }
    captured
}

#[tokio::test]
async fn native_mcp_iserror_presence_round_trips_as_exact_model_tool_result_bytes() {
    let native_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(MCP_PRESENCE_NATIVE_DIR);
    let initialize_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(MCP_PRESENCE_INITIALIZE_DIR);
    let cases: Value = serde_json::from_slice(&read_fixture(&native_dir, "cases.json"))
        .expect("Native isError cases JSON");
    let native_stdout: Value =
        serde_json::from_slice(&read_fixture(&native_dir, "native.stdout.raw.json"))
            .expect("Native MCP isError CLI result");
    assert_eq!(
        native_stdout["result"],
        "NATIVE_MCP_TOOL_RESULT_PRESENCE_FINISHED"
    );
    assert_eq!(native_stdout["num_turns"], 4);

    let expected_results = [
        (
            "result_omitted",
            "toolu_native_iserror_omitted_295",
            "model-tool-result-result_omitted.raw.json",
        ),
        (
            "result_false",
            "toolu_native_iserror_false_295",
            "model-tool-result-result_false.raw.json",
        ),
        (
            "result_true",
            "toolu_native_iserror_true_295",
            "model-tool-result-result_true.raw.json",
        ),
    ];
    for index in 1..=4 {
        assert_fixed_usage(&native_dir, &format!("model-response-{index:02}.raw.sse"));
    }
    for (name, tool_use_id, raw_name) in expected_results {
        let expected = read_fixture(&native_dir, raw_name);
        let expected_source = Utf16JsonProjection::parse(
            std::str::from_utf8(&expected).expect("Native model tool_result raw bytes are UTF-8"),
        )
        .expect("Native model tool_result JSON");
        assert_eq!(expected_source.string_at("/type"), Some("tool_result"));
        assert_eq!(expected_source.string_at("/tool_use_id"), Some(tool_use_id));
        match name {
            "result_omitted" | "result_false" => {
                assert!(expected_source.at_pointer("/is_error").is_none())
            }
            "result_true" => assert_eq!(
                expected_source
                    .at_pointer("/is_error")
                    .and_then(|v| v.as_bool()),
                Some(true)
            ),
            _ => unreachable!(),
        }
    }

    let (mcp_connection, mcp_response_tx, mcp_request_rx) = paired_mcp_connection();
    let mcp_peer_task = tokio::spawn(mcp_peer_iserror_cases(
        native_dir.clone(),
        initialize_dir,
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
        3,
        "three Native fixture tools are discovered"
    );
    let names = discovered_tools
        .iter()
        .map(|tool| tool.full_name.as_str())
        .collect::<Vec<_>>();
    assert!(names.iter().any(|name| name.ends_with("result_omitted")));
    assert!(names.iter().any(|name| name.ends_with("result_false")));
    assert!(names.iter().any(|name| name.ends_with("result_true")));

    let mcp_registry = Arc::new(McpRegistry::new(Arc::new(UnusedMcpTransport)));
    mcp_registry.register_client("fixture", mcp_client).await;
    let temp = tempdir().expect("isolated Orchestrator session root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create isolated cwd");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let session_path = temp.path().join("session.jsonl");
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));
    let tools = Arc::new(ToolRegistry::new());
    let mut mcp_tools = Vec::<Arc<dyn Tool>>::new();
    for discovered in discovered_tools {
        let mut tool_context = tool_api::test_support::ctx_for_file_tools(
            fs.clone(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![workspace.clone()],
        );
        tool_context.mcp_registry = Some(mcp_registry.clone());
        let tool = Arc::new(MCPTool::new_for_tool(
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
        mcp_tools.push(tool);
    }
    tools.register_mcp_tools(McpConnectionId::new(), mcp_tools);

    let transport = Arc::new(FixedAnthropicTransport {
        responses: Mutex::new(VecDeque::from(
            (1..=4)
                .map(|index| {
                    read_fixture(&native_dir, &format!("model-response-{index:02}.raw.sse"))
                })
                .collect::<Vec<_>>(),
        )),
        requests: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let session_id = SessionId::from_uuid(
        uuid::Uuid::parse_str(MCP_PRESENCE_SESSION_ID).expect("fixed Native isError session id"),
    );
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 4,
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            workspace,
        )
        .with_session_id(session_id)
        .with_jsonl_writer(writer)
        .with_mcp_registry(mcp_registry),
    );
    let prompt = cases["prompt"].as_str().expect("fixed user prompt");
    let outcome = orchestrator
        .run_turn_streaming(prompt)
        .await
        .expect("fixed MCP isError roundtrip");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let captured_mcp_requests = mcp_peer_task.await.expect("MCP fixture peer task");
    assert_eq!(captured_mcp_requests.len(), 5);
    let methods = captured_mcp_requests
        .iter()
        .map(|raw| {
            let request: Value = serde_json::from_slice(raw).expect("captured MCP request JSON");
            request["method"].as_str().expect("MCP method").to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "initialize",
            "tools/list",
            "tools/call",
            "tools/call",
            "tools/call"
        ]
    );

    let sdk_requests = transport
        .requests
        .lock()
        .expect("captured SDK request lock")
        .clone();
    assert_eq!(
        sdk_requests.len(),
        4,
        "one SDK request per Native SSE fixture"
    );
    assert!(transport
        .responses
        .lock()
        .expect("fixed Native SSE queue lock")
        .is_empty());
    let observed_cases = [
        (
            "result_omitted",
            "toolu_native_iserror_omitted_295",
            "model-tool-result-result_omitted.raw.json",
        ),
        (
            "result_false",
            "toolu_native_iserror_false_295",
            "model-tool-result-result_false.raw.json",
        ),
        (
            "result_true",
            "toolu_native_iserror_true_295",
            "model-tool-result-result_true.raw.json",
        ),
    ];
    for (name, use_id, native_filename) in observed_cases {
        let candidate = sdk_requests
            .iter()
            .find_map(|request| raw_tool_result_by_id(request, use_id))
            .unwrap_or_else(|| panic!("SDK requests contain tool_result for {name} ({use_id})"));
        let native = read_fixture(&native_dir, native_filename);
        assert_eq!(
            candidate, native,
            "full raw SDK model tool_result must match the Native 2.1.295 bytes for {name}; no field stripping or reserialization",
        );
    }

    let candidate_session =
        std::fs::read_to_string(&session_path).expect("candidate MCP session JSONL");
    let candidate_error_row = candidate_session
        .lines()
        .filter_map(|line| Utf16JsonProjection::parse(line).ok())
        .find(|row| {
            row.string_at("/type") == Some("user")
                && row.string_at("/message/content/0/type") == Some("tool_result")
                && row.string_at("/message/content/0/tool_use_id")
                    == Some("toolu_native_iserror_true_295")
        })
        .expect("candidate session persists the Native MCP failure result");
    let native_session = read_fixture(&native_dir, "native-session.raw.jsonl");
    let native_error_row = std::str::from_utf8(&native_session)
        .expect("Native session JSONL is UTF-8")
        .lines()
        .filter_map(|line| Utf16JsonProjection::parse(line).ok())
        .find(|row| {
            row.string_at("/type") == Some("user")
                && row.string_at("/message/content/0/type") == Some("tool_result")
                && row.string_at("/message/content/0/tool_use_id")
                    == Some("toolu_native_iserror_true_295")
        })
        .expect("Native session contains the MCP failure result");
    assert_eq!(
        candidate_error_row.string_units("/message/content/0/content"),
        native_error_row.string_units("/message/content/0/content"),
        "model-facing content is the bare MCP server detail"
    );
    assert_eq!(
        candidate_error_row
            .at_pointer("/message/content/0/is_error")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        candidate_error_row.string_units("/toolUseResult"),
        native_error_row.string_units("/toolUseResult"),
        "UI toolUseResult keeps Native's separate Error: prefix"
    );
}
