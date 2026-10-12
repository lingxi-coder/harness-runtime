//! Replay captured Native MCP results through the production JSON-RPC client,
//! registry, and `MCPTool::call` path. The structured-content case is pinned
//! to 2.1.293; the MCP error-detail matrix is pinned to 2.1.295. Error cases
//! stop at the actual ToolError model-facing projection.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::unfold;
use jsonrpc::{Connection, LineCodec};
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
    McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::McpConnectionId;
use mcp::{McpClient, McpRegistry};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tool_api::tool_trait::{Tool, ToolError};
use tool_mcp::MCPTool;

const NATIVE_TOOL_RESULT: &str = include_str!(
    "../../../../docs/parity/current-alignment/fixtures/mcp-roundtrip/native-baseline/structured-content-lone/model-tool-result.raw.json"
);
const NATIVE_TOOLS_CALL_RESPONSE: &str = include_str!(
    "../../../../docs/parity/current-alignment/fixtures/mcp-roundtrip/native-baseline/structured-content-lone/mcp-tools-call-response.raw.jsonrpc"
);
const FIXED_TOOL_USE_ID: &str = "toolu_native_mcp_fixture_1";

// Exact tools/call JSON-RPC response lines from
// acceptance/native-mcp-error-detail-295-capture/attempt-01/cases/*/trace/
// tools-call-01-response.raw.jsonrpc. The fixture is tied to the six-case,
// one-shot 2.1.295 capture receipt; only the dynamic JSON-RPC id is replaced.
const NATIVE_295_ERROR_CASES: &[(&str, &str, &str)] = &[
    (
        "empty_content_error_null",
        r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","content":[],"isError":true,"error":null}}"#,
        "null",
    ),
    (
        "empty_content_error_number",
        r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","content":[],"isError":true,"error":17.25}}"#,
        "17.25",
    ),
    (
        "empty_content_error_array",
        r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","content":[],"isError":true,"error":[1,null,"x",{"k":0}]}}"#,
        "1,,x,[object Object]",
    ),
    (
        "multiblock_text_utf16",
        r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","content":[{"type":"text","text":"DETAIL_FIRST_\ud800"},{"type":"image","data":"ignored","mimeType":"image/png"},{"type":"text","text":"DETAIL_SECOND"}],"isError":true}}"#,
        "DETAIL_FIRST_\nDETAIL_SECOND",
    ),
    (
        "resource_link_text_precedence",
        r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","content":[{"type":"resource_link","name":"guide","uri":"mcp://guide","description":"available"},{"type":"resource_link","name":"empty","uri":"mcp://empty","description":""},{"type":"resource_link","name":"shadowed","uri":"mcp://shadowed","description":"truthy","text":"TEXT_PROPERTY_PRECEDES_RESOURCE_LINK"}],"isError":true}}"#,
        "[Resource link: guide] mcp://guide (available)\n[Resource link: empty] mcp://empty\n[Resource link: shadowed] mcp://shadowed (truthy)",
    ),
    (
        "empty_content_error_to_string_shadow",
        r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","content":[],"isError":true,"error":{"toString":null}}}"#,
        "No default value",
    ),
];

struct UnusedTransport;

#[async_trait]
impl McpTransport for UnusedTransport {
    async fn connect(&self, _spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        unreachable!("the test registers an already-connected client")
    }
    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        unreachable!("the test registers an already-connected client")
    }
    async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn list_prompts(&self, _conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        _tool: &str,
        _input: Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!("the test drives the registered JSON-RPC client directly")
    }
    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn ping(&self, _id: McpConnectionId) -> Result<(), McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        unreachable!("the test exercises the call/result path only")
    }
    async fn disconnect(&self, _id: McpConnectionId) -> Result<(), McpError> {
        unreachable!("the test registers an already-connected client")
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio]
    }
}

fn paired() -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
    let (peer_to_client_tx, peer_to_client_rx) = mpsc::channel::<Bytes>(8);
    let (client_to_peer_tx, client_to_peer_rx) = mpsc::channel::<Bytes>(8);
    let inbound = Box::pin(unfold(peer_to_client_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|frame| (Ok::<Bytes, std::io::Error>(frame), rx))
    }));
    let outbound = Box::pin(futures::sink::unfold(
        client_to_peer_tx,
        |tx, frame: Bytes| async move {
            tx.send(frame)
                .await
                .map_err(|_| std::io::Error::other("test peer closed"))?;
            Ok::<_, std::io::Error>(tx)
        },
    ));
    let connection = Arc::new(Connection::builder(LineCodec::default()).build(inbound, outbound));
    (connection, peer_to_client_tx, client_to_peer_rx)
}

#[tokio::test]
async fn native_structured_content_result_survives_production_mcp_tool_path() {
    let (connection, response_tx, mut request_rx) = paired();
    let (captured_tx, captured_rx) = oneshot::channel::<Vec<u8>>();
    let (response_text_tx, response_text_rx) = oneshot::channel::<String>();
    let responder = tokio::spawn(async move {
        let request_frame = request_rx
            .recv()
            .await
            .expect("MCP client sends tools/call");
        let request: Value = serde_json::from_slice(&request_frame).expect("request JSON");
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], "emit_payload");
        assert_eq!(
            request["params"]["arguments"],
            json!({"mode":"source-units"})
        );
        assert_eq!(
            request["params"]["_meta"]["claudecode/toolUseId"],
            FIXED_TOOL_USE_ID
        );
        let _ = captured_tx.send(request_frame.to_vec());

        // Reuse the exact captured Native JSON-RPC result bytes; only echo the
        // request's dynamic JSON-RPC id, as the protocol requires.
        let response_id = request["id"].to_string();
        let response =
            NATIVE_TOOLS_CALL_RESPONSE.replacen("\"id\":2", &format!("\"id\":{response_id}"), 1);
        let _ = response_text_tx.send(response.clone());
        response_tx
            .send(Bytes::from(format!("{response}\n")))
            .await
            .expect("return Native result frame to MCP client");
        request
    });

    let client = Arc::new(McpClient::new("fixture", PathBuf::from("/tmp"), connection).await);
    let registry = Arc::new(McpRegistry::new(Arc::new(UnusedTransport)));
    registry.register_client("fixture", client).await;

    let mut builtin = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![PathBuf::from("/tmp")],
    );
    builtin.mcp_registry = Some(registry);
    let tool = MCPTool::new_for_tool(
        builtin,
        "mcp__fixture__emit_payload".into(),
        Utf16JsonProjection::parse("\"Fixture payload tool\"").expect("description JSON"),
        Utf16JsonProjection::parse(r#"{"type":"object"}"#).expect("schema JSON"),
        None,
        None,
        None,
        true,
        false,
    );
    let mut use_context = tool_api::test_support::fresh_ctx();
    use_context.tool_use_id = Some(FIXED_TOOL_USE_ID.into());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.call(
            Utf16JsonProjection::parse(r#"{"mode":"source-units"}"#).expect("fixed tool arguments"),
            use_context,
            tool_api::test_support::fresh_tx(),
        ),
    )
    .await
    .expect("fixed MCP response must finish within five seconds")
    .expect("production MCP tool result transformation");
    let request = responder.await.expect("MCP responder task");
    let response_sent = response_text_rx
        .await
        .expect("captured raw response sent to the candidate client");
    let captured_request = captured_rx.await.expect("captured raw MCP request");
    assert_eq!(
        serde_json::from_slice::<Value>(&captured_request).expect("captured request JSON"),
        request,
        "the captured raw request is the request answered by the fixture peer"
    );

    let native: Value = serde_json::from_str(NATIVE_TOOL_RESULT).expect("Native tool result JSON");
    let native_content = native["content"]
        .as_str()
        .expect("Native model content string");
    let (native_fixed_content, native_dynamic_reminder) = native_content
        .split_once("\n\n<system-reminder>\n")
        .expect("separate documented Native dynamic reminder suffix");
    assert!(native_dynamic_reminder.starts_with("<total_tokens>"));
    let native_fixed_units = native_fixed_content.encode_utf16().collect::<Vec<_>>();

    let model_content = result
        .model_content
        .as_ref()
        .expect("actual MCPTool supplies faithful model-facing content");
    assert_eq!(
        model_content.string_units(""),
        Some(native_fixed_units.clone()),
        "candidate model-facing result matches Native's exact fixed content before its separately dynamic reminder"
    );
    assert_eq!(
        result.data.string_units(""),
        Some(native_fixed_units),
        "the actual ToolCallResult data preserves the Native fixed content"
    );
    let meta = result
        .mcp_meta
        .as_ref()
        .expect("structuredContent metadata");
    let expected_label = "structured-"
        .encode_utf16()
        .chain([0xd800, 0x03a9])
        .collect::<Vec<_>>();
    assert_eq!(
        meta.string_units("/structuredContent/label"),
        Some(expected_label),
        "structuredContent retains the source lone UTF-16 unit"
    );
    assert!(!result.is_error);

    println!(
        "CURRENT_ALIGNMENT_NATIVE_MCP_RESULT={}",
        json!({
            "request": request,
            "request_raw": String::from_utf8_lossy(&captured_request),
            "response_raw_with_candidate_id": response_sent,
            "dynamic_candidate_fields": ["JSON-RPC id", "progressToken"],
            "model_content_utf16_units": model_content.string_units("").expect("model text units"),
            "structured_label_utf16_units": meta.string_units("/structuredContent/label").expect("label units"),
            "native_dynamic_reminder_separate": true,
            "is_error": result.is_error
        })
    );
}

#[tokio::test]
async fn native_295_error_details_survive_transport_and_tool_model_projection() {
    for (case_name, response_fixture, expected_text) in NATIVE_295_ERROR_CASES {
        let (connection, response_tx, mut request_rx) = paired();
        let (response_text_tx, response_text_rx) = oneshot::channel::<String>();
        let response_fixture = (*response_fixture).to_owned();
        let responder = tokio::spawn(async move {
            let request_frame = request_rx
                .recv()
                .await
                .expect("MCP client sends tools/call");
            let request: Value = serde_json::from_slice(&request_frame).expect("request JSON");
            assert_eq!(request["jsonrpc"], "2.0");
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], "emit_error");
            assert_eq!(request["params"]["arguments"], json!({}));
            let response_id = request["id"].to_string();
            let response =
                response_fixture.replacen("\"id\":3", &format!("\"id\":{response_id}"), 1);
            assert_ne!(
                response, response_fixture,
                "the captured response's fixed JSON-RPC id is replaced per call"
            );
            response_text_tx
                .send(response.clone())
                .expect("capture response before sending it to the MCP client");
            response_tx
                .send(Bytes::from(format!("{response}\n")))
                .await
                .expect("return captured Native result frame");
            request
        });

        let client = Arc::new(McpClient::new("fixture", PathBuf::from("/tmp"), connection).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(UnusedTransport)));
        registry.register_client("fixture", client).await;

        let mut builtin = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![PathBuf::from("/tmp")],
        );
        builtin.mcp_registry = Some(registry);
        let tool = MCPTool::new_for_tool(
            builtin,
            "mcp__fixture__emit_error".into(),
            Utf16JsonProjection::from("Fixture MCP error tool"),
            Utf16JsonProjection::parse(r#"{"type":"object"}"#).expect("schema JSON"),
            None,
            None,
            None,
            true,
            false,
        );
        let tool_use_id = format!("toolu_native_mcp_error_detail_295_{case_name}");
        let mut use_context = tool_api::test_support::fresh_ctx();
        use_context.tool_use_id = Some(tool_use_id.into());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool.call(
                Utf16JsonProjection::parse("{}").expect("arguments JSON"),
                use_context,
                tool_api::test_support::fresh_tx(),
            ),
        )
        .await
        .expect("captured MCP error response must finish within five seconds");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("captured isError result unexpectedly became success: {case_name}"),
        };
        let request = responder.await.expect("MCP fixture responder task");
        assert_eq!(request["params"]["name"], "emit_error");
        let response_sent = response_text_rx
            .await
            .expect("captured Native MCP response was sent");
        assert!(
            response_sent.contains("\"isError\":true"),
            "the production client received the captured logical error response"
        );

        let mut expected_units = expected_text.encode_utf16().collect::<Vec<_>>();
        if *case_name == "multiblock_text_utf16" {
            expected_units = "DETAIL_FIRST_"
                .encode_utf16()
                .chain([0xd800])
                .chain("\nDETAIL_SECOND".encode_utf16())
                .collect();
            assert!(
                response_sent.contains(r#"DETAIL_FIRST_\ud800"#),
                "the raw response fixture preserves the captured lone surrogate spelling"
            );
        }
        if *case_name == "empty_content_error_null" {
            assert!(
                response_sent.contains("\"error\":null"),
                "the fixture carries explicit error:null through transport decoding"
            );
        }
        if *case_name == "resource_link_text_precedence" {
            assert!(
                response_sent.contains("TEXT_PROPERTY_PRECEDES_RESOURCE_LINK"),
                "the raw resource_link fixture retains its extra text property"
            );
        }

        let model_facing = error.model_facing_message();
        assert_eq!(
            model_facing.string_units(""),
            Some(expected_units.clone()),
            "{case_name}: final MCPTool ToolError projection matches the captured Native detail"
        );
        match error {
            ToolError::McpFailure {
                server,
                tool,
                detail,
            } => {
                assert_eq!(server, "fixture", "{case_name}: source server identity");
                assert_eq!(tool, "emit_error", "{case_name}: source tool identity");
                assert_eq!(
                    detail.string_units(""),
                    Some(expected_units),
                    "{case_name}: typed MCP failure retains the exact detail units"
                );
            }
            other => panic!("{case_name}: expected typed MCP failure, got {other:?}"),
        }
    }
}
