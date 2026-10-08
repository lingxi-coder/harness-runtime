//! Replay the pinned Native 2.1.293 MCP result through the production
//! `McpClient` + `MCPTool::call` path. This stops at the actual tool result
//! source boundary; the full model/Orchestrator loop is a separate gate.

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
use tool_api::tool_trait::Tool;
use tool_mcp::MCPTool;

const NATIVE_TOOL_RESULT: &str = include_str!(
    "../../../../docs/parity/current-alignment/fixtures/mcp-roundtrip/native-baseline/structured-content-lone/model-tool-result.raw.json"
);
const NATIVE_TOOLS_CALL_RESPONSE: &str = include_str!(
    "../../../../docs/parity/current-alignment/fixtures/mcp-roundtrip/native-baseline/structured-content-lone/mcp-tools-call-response.raw.jsonrpc"
);
const FIXED_TOOL_USE_ID: &str = "toolu_native_mcp_fixture_1";

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
