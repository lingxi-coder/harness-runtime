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
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) const MODEL: &str = "claude-sonnet-4-5";

pub(crate) struct FixedAnthropicTransport {
    pub(crate) responses: Mutex<VecDeque<Vec<u8>>>,
    pub(crate) requests: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl Transport for FixedAnthropicTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.requests
            .lock()
            .expect("captured SDK request lock")
            .push(request.body.to_vec());
        let body = self
            .responses
            .lock()
            .expect("fixed Native SSE queue lock")
            .pop_front()
            .expect("fixed Native SSE queue has a response");
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into(),
        }
        .into())
    }
}

pub(crate) fn api_service(transport: Arc<FixedAnthropicTransport>) -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": "https://api.anthropic.com",
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": false,
                    "structured_output": false
                }
            }]
        }]
    }))
    .expect("fixed Native Anthropic route config");

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "current-alignment-native-mcp-roundtrip",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled),
    )
}

pub(crate) struct UnusedMcpTransport;

#[async_trait]
impl McpTransport for UnusedMcpTransport {
    async fn connect(&self, _spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        unreachable!("the fixture registers the initialized MCP client")
    }
    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        unreachable!("the fixture initializes through McpClient")
    }
    async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        unreachable!("the fixture lists tools through McpClient")
    }
    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        unreachable!("the fixture has no resource tools")
    }
    async fn list_prompts(&self, _conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        unreachable!("the fixture has no prompt tools")
    }
    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        _tool: &str,
        _input: Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!("the actual McpClient handles tools/call")
    }
    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!("the fixture has no resources")
    }
    async fn ping(&self, _id: McpConnectionId) -> Result<(), McpError> {
        unreachable!("the fixture does not ping")
    }
    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!("the fixture has no notifications")
    }
    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        unreachable!("the fixture does not elicit")
    }
    async fn disconnect(&self, _id: McpConnectionId) -> Result<(), McpError> {
        unreachable!("the fixture keeps its local JSON-RPC connection alive")
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio]
    }
}

pub(crate) fn paired_mcp_connection() -> (
    Arc<Connection>,
    tokio::sync::mpsc::Sender<Bytes>,
    tokio::sync::mpsc::Receiver<Bytes>,
) {
    let (server_to_client_tx, server_to_client_rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    let (client_to_server_tx, client_to_server_rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    let inbound = Box::pin(unfold(server_to_client_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|frame| (Ok::<Bytes, std::io::Error>(frame), rx))
    }));
    let outbound = Box::pin(futures::sink::unfold(
        client_to_server_tx,
        |tx, frame: Bytes| async move {
            tx.send(frame)
                .await
                .map_err(|_| std::io::Error::other("fixture MCP peer closed"))?;
            Ok::<_, std::io::Error>(tx)
        },
    ));
    let connection = Arc::new(Connection::builder(LineCodec::default()).build(inbound, outbound));
    (connection, server_to_client_tx, client_to_server_rx)
}

pub(crate) fn read_fixture(fixture_dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(fixture_dir.join(name)).unwrap_or_else(|error| {
        panic!(
            "read Native fixture {}: {error}",
            fixture_dir.join(name).display()
        )
    })
}

pub(crate) fn assert_fixed_usage(fixture_dir: &Path, name: &str) {
    let raw = std::fs::read_to_string(fixture_dir.join(name)).expect("Native SSE fixture");
    let mut start_usage = None;
    let mut delta_usage = None;
    for line in raw.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let value: Value = serde_json::from_str(data).expect("SSE event JSON");
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                start_usage = value.pointer("/message/usage").cloned();
            }
            Some("message_delta") => delta_usage = value.get("usage").cloned(),
            _ => {}
        }
    }
    assert_eq!(
        start_usage
            .as_ref()
            .and_then(|usage| usage["input_tokens"].as_u64()),
        Some(10)
    );
    assert_eq!(
        delta_usage
            .as_ref()
            .and_then(|usage| usage["output_tokens"].as_u64()),
        Some(5),
        "fixed Native usage remains part of the roundtrip oracle"
    );
}

pub(crate) fn find_tool_result(
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
