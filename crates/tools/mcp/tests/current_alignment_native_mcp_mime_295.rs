//! Exercise the Native 2.1.295 embedded-resource receipt through the live
//! MCP JSON-RPC client and production MCP tool transformation/persistence.

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

const NATIVE_TOOLS_CALL_RESPONSE: &str = include_str!(
    "../../../../docs/parity/current-alignment/fixtures/mcp-mime-295/native-baseline/2.1.295/mcp-jsonrpc/tools-call-response-01.raw.jsonrpc"
);
const NATIVE_CASES: &str =
    include_str!("../../../../docs/parity/current-alignment/fixtures/mcp-mime-295/cases.json");
const TOOL_USE_ID: &str = "toolu_native_mcp_mime_295_blob_matrix";

struct UnusedTransport;

#[async_trait]
impl McpTransport for UnusedTransport {
    async fn connect(&self, _spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        unreachable!("the fixture registers an already-connected MCP client")
    }
    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        unreachable!("the fixture drives the connected JSON-RPC client")
    }
    async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn list_prompts(&self, _conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        _tool: &str,
        _input: Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!("the fixture drives the connected JSON-RPC client")
    }
    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn ping(&self, _id: McpConnectionId) -> Result<(), McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        unreachable!("the fixture exercises tools/call only")
    }
    async fn disconnect(&self, _id: McpConnectionId) -> Result<(), McpError> {
        unreachable!("the fixture registers an already-connected MCP client")
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
                .map_err(|_| std::io::Error::other("fixture peer closed"))?;
            Ok::<_, std::io::Error>(tx)
        },
    ));
    let connection = Arc::new(Connection::builder(LineCodec::default()).build(inbound, outbound));
    (connection, peer_to_client_tx, client_to_peer_rx)
}

fn decode_hex(input: &str) -> Vec<u8> {
    assert_eq!(input.len() % 2, 0, "fixture hex has whole bytes");
    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("fixture hex is ASCII");
            u8::from_str_radix(pair, 16).expect("fixture contains hexadecimal bytes")
        })
        .collect()
}

#[tokio::test]
async fn native_295_mcp_tool_resource_blobs_use_mime_extensions_and_exact_bytes() {
    let (connection, response_tx, mut request_rx) = paired();
    let (captured_tx, captured_rx) = oneshot::channel::<Vec<u8>>();
    let responder = tokio::spawn(async move {
        let request_frame = request_rx
            .recv()
            .await
            .expect("MCP client sends tools/call");
        let request: Value = serde_json::from_slice(&request_frame).expect("request JSON");
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], "return_blob_matrix");
        assert_eq!(request["params"]["arguments"], json!({}));
        assert_eq!(
            request["params"]["_meta"]["claudecode/toolUseId"],
            TOOL_USE_ID
        );
        let response_id = request["id"].to_string();
        let response =
            NATIVE_TOOLS_CALL_RESPONSE.replacen("\"id\":4", &format!("\"id\":{response_id}"), 1);
        assert_ne!(
            response, NATIVE_TOOLS_CALL_RESPONSE,
            "replace dynamic RPC id"
        );
        captured_tx
            .send(request_frame.to_vec())
            .expect("capture request before returning Native response");
        response_tx
            .send(Bytes::from(format!("{response}\n")))
            .await
            .expect("return exact Native tools/call result frame");
    });

    let client = Arc::new(McpClient::new("fixture", PathBuf::from("/tmp"), connection).await);
    let registry = Arc::new(McpRegistry::new(Arc::new(UnusedTransport)));
    registry.register_client("fixture", client).await;

    let temporary = tempfile::tempdir().expect("isolated tool output root");
    let workspace = temporary.path().join("workspace");
    let mut builtin = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![workspace],
    );
    let output_dir = builtin.tool_results_dir();
    builtin.mcp_registry = Some(registry);
    let tool = MCPTool::new_for_tool(
        builtin,
        "mcp__fixture__return_blob_matrix".into(),
        Utf16JsonProjection::from("Native MIME matrix"),
        Utf16JsonProjection::parse(r#"{"type":"object"}"#).expect("schema JSON"),
        None,
        None,
        None,
        true,
        false,
    );
    let mut use_context = tool_api::test_support::fresh_ctx();
    use_context.tool_use_id = Some(TOOL_USE_ID.into());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.call(
            Utf16JsonProjection::parse("{}").expect("empty arguments JSON"),
            use_context,
            tool_api::test_support::fresh_tx(),
        ),
    )
    .await
    .expect("Native MIME matrix result completes")
    .expect("production MCP tool transformation succeeds");
    responder.await.expect("fixture responder task");
    let captured_request = captured_rx.await.expect("captured JSON-RPC request");
    let request: Value = serde_json::from_slice(&captured_request).expect("captured request JSON");
    assert_eq!(request["params"]["name"], "return_blob_matrix");
    assert_eq!(request["params"]["arguments"], json!({}));
    assert!(!result.is_error);

    let native_cases: Value = serde_json::from_str(NATIVE_CASES).expect("Native cases JSON");
    let cases = native_cases["resources"]
        .as_array()
        .expect("sixteen Native MIME cases");
    let expected_extensions = [
        "ico",
        "ico",
        "css",
        "js",
        "js",
        "xml",
        "xml",
        "webmanifest",
        "woff2",
        "woff2",
        "woff",
        "woff",
        "ttf",
        "otf",
        "wasm",
        "bin",
    ];
    assert_eq!(cases.len(), expected_extensions.len());
    assert_eq!(result.data.array_len(""), Some(cases.len()));

    let mut expected_model_units = Vec::new();
    for (index, (case, expected_extension)) in cases.iter().zip(expected_extensions).enumerate() {
        let uri = case["uri"].as_str().expect("fixture URI");
        let mime = case["mime"].as_str().expect("fixture MIME");
        let bytes_hex = case["bytes_hex"].as_str().expect("fixture binary bytes");
        let text = result
            .data
            .string_at(&format!("/{index}/text"))
            .expect("production transformation emits saved-blob text");
        let expected_prefix = format!("[Resource from fixture at {uri}] Binary content ({mime},");
        assert!(
            text.starts_with(&expected_prefix),
            "resource message: {text}"
        );
        let saved_path = text
            .rsplit_once(" saved to ")
            .map(|(_, path)| path)
            .expect("saved-blob message includes its path");
        let path = std::path::Path::new(saved_path);
        assert!(
            path.starts_with(&output_dir),
            "saved under test output dir: {path:?}"
        );
        assert_eq!(
            path.extension().and_then(|ext| ext.to_str()),
            Some(expected_extension),
            "{mime}"
        );
        assert_eq!(
            std::fs::read(path).expect("persisted MIME fixture blob"),
            decode_hex(bytes_hex),
            "raw bytes for {mime}"
        );

        if index > 0 {
            expected_model_units.push(b'\n' as u16);
        }
        expected_model_units.extend(text.encode_utf16());
    }
    assert_eq!(
        result
            .model_content
            .as_ref()
            .and_then(|content| content.string_units("")),
        Some(expected_model_units.as_slice()),
        "all persisted resource messages reach the model in Native order"
    );
}
