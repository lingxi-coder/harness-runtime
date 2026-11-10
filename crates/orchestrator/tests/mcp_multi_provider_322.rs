//! MCP remains independent of the LLM provider during Claude compatibility work.
//! Exercise discovery, provider tool-call decoding, MCP execution and the next
//! provider request through real codecs. All responses are local fixtures.

mod common;

use async_trait::async_trait;
use bytes::Bytes;
use common::{paired_mcp_connection, UnusedMcpTransport};
use lingxi_core::host::FileSystem;
use lingxi_core::host::OutputEvent;
use lingxi_core::types::McpConnectionId;
use llm_runtime::{
    model::{thinking::ThinkingConfig, user_agent::UserAgentEnv},
    ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport,
};
use mcp::{McpClient, McpRegistry};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tool_api::{registry::ToolRegistry, tool_trait::Tool};
use tool_mcp::MCPTool;

const MODEL: &str = "provider-neutral-model";
const TOOL: &str = "mcp__multi_provider__echo";
const CALL_ID: &str = "provider-call-322";
const ARGUMENT: &str = "café + Ω/😀";
// The escaped surrogate is inside structured JSON text, as in MCP's native
// structuredContent projection; every provider must retain these exact units.
const RESULT: &str = r#"{"ok":true,"label":"reply-\ud800Ω"}"#;

#[derive(Clone, Copy, Debug)]
enum Family {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
    Gemini,
    BedrockClaude,
    VertexClaude,
    FoundryClaude,
    AzureOpenAi,
    GeminiInteractions,
    VertexGemini,
}

impl Family {
    fn route(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Anthropic => (
                "anthropic_first_party",
                "anthropic_messages",
                "https://api.anthropic.com",
            ),
            Self::OpenAiChat => ("open_ai", "open_ai_chat", "https://api.openai.com/v1"),
            Self::OpenAiResponses => ("open_ai", "open_ai_responses", "https://api.openai.com/v1"),
            Self::Gemini => (
                "gemini",
                "gemini_generate_content",
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            Self::BedrockClaude => ("bedrock_claude", "bedrock_claude", "https://bedrock-runtime.us-east-1.amazonaws.com"),
            Self::VertexClaude => ("vertex_claude", "vertex_claude", "https://us-central1-aiplatform.googleapis.com/v1/projects/test/locations/us-central1"),
            Self::FoundryClaude => ("foundry_claude", "foundry_claude", "https://example.services.ai.azure.com/anthropic"),
            Self::AzureOpenAi => ("azure_open_ai", "azure_open_ai", "https://example.openai.azure.com"),
            Self::GeminiInteractions => ("gemini", "gemini_interactions", "https://generativelanguage.googleapis.com/v1beta"),
            Self::VertexGemini => ("vertex_gemini", "vertex_gemini", "https://us-central1-aiplatform.googleapis.com/v1/projects/test/locations/us-central1"),
        }
    }

    fn responses(self) -> [Value; 2] {
        let args = json!({"text": ARGUMENT});
        match self {
            Self::Anthropic | Self::BedrockClaude | Self::VertexClaude | Self::FoundryClaude => [
                json!({"id":"first","type":"message","role":"assistant","model":MODEL,
                    "content":[{"type":"tool_use","id":CALL_ID,"name":TOOL,"input":args}],
                    "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":2}}),
                json!({"id":"final","type":"message","role":"assistant","model":MODEL,
                    "content":[{"type":"text","text":"done"}],"stop_reason":"end_turn",
                    "usage":{"input_tokens":5,"output_tokens":2}}),
            ],
            Self::OpenAiChat | Self::AzureOpenAi => [
                json!({"id":"first","object":"chat.completion","model":MODEL,"choices":[{
                    "index":0,"message":{"role":"assistant","content":null,"tool_calls":[{
                        "id":CALL_ID,"type":"function","function":{"name":TOOL,"arguments":args.to_string()}}]},
                    "finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}),
                json!({"id":"final","object":"chat.completion","model":MODEL,"choices":[{
                    "index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}),
            ],
            Self::OpenAiResponses => [
                json!({"id":"first","model":MODEL,"status":"completed","output":[{
                    "type":"function_call","id":"fc-first","call_id":CALL_ID,"name":TOOL,
                    "arguments":args.to_string(),"status":"completed"}],
                    "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}),
                json!({"id":"final","model":MODEL,"status":"completed","output":[{
                    "type":"message","role":"assistant","content":[{"type":"output_text","text":"done","annotations":[]}]}],
                    "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}),
            ],
            Self::Gemini | Self::VertexGemini => [
                json!({"candidates":[{"content":{"role":"model","parts":[{
                    "functionCall":{"id":CALL_ID,"name":TOOL,"args":args}}]},"finishReason":"STOP"}],
                    "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}),
                json!({"candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}],
                    "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}),
            ],
            Self::GeminiInteractions => [
                json!({"id":"first","model":MODEL,"status":"requires_action","steps":[{
                    "type":"function_call","id":CALL_ID,"name":TOOL,"arguments":args}]}),
                json!({"id":"final","model":MODEL,"status":"completed","steps":[{
                    "type":"model_output","content":[{"type":"text","text":"done"}]}]}),
            ],
        }
    }
}

struct ProviderFixture {
    responses: Mutex<VecDeque<Value>>,
    requests: Mutex<Vec<Value>>,
    request_shapes: Mutex<Vec<Value>>,
}

#[async_trait]
impl Transport for ProviderFixture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        self.request_shapes.lock().unwrap().push(json!({
            "url":request.url,"keys":body.as_object().unwrap().keys().collect::<Vec<_>>(),
            "model":body["model"],"stream":body["stream"]
        }));
        if request
            .url
            .split('?')
            .next()
            .is_some_and(|url| url.ends_with("/messages/count_tokens"))
        {
            return Ok(lingxi_llm_client::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: Bytes::from_static(br#"{"input_tokens":128}"#),
            }
            .into());
        }
        self.requests.lock().unwrap().push(body);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("only two fixture provider requests");
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&response).unwrap().into(),
        }
        .into())
    }
}

fn service(family: Family, transport: Arc<ProviderFixture>) -> Arc<ApiService> {
    let (provider, protocol, endpoint) = family.route();
    let mut host_config = json!({"providers":[{
        "provider_id":provider,"profile_name":"fixture","base_url":endpoint,"protocol":protocol,
        "auth":"none","credential":{"type":"none"},"models":[{
            "display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,
            "capabilities":{"streaming":true,"tools":true,"reasoning":false,
                "vision":false,"documents":false,"structured_output":false}}]}]});
    if matches!(family, Family::AzureOpenAi) {
        host_config["providers"][0]["azure"] = json!({"apiVersion":"2024-02-01"});
    }
    if matches!(family, Family::GeminiInteractions) {
        host_config["providers"][0]["auth"] = json!("api_key");
        host_config["providers"][0]["credential"] = json!({"type":"static","id":"fixture-key"});
    }
    let mut config: ClientConfig =
        serde_json::from_value(host_config).expect("provider fixture config");
    // Unknown Messages models need an explicit application output budget.
    // Retain the admitted SDK setting through the host's wire-profile seam.
    if matches!(
        family,
        Family::Anthropic | Family::BedrockClaude | Family::VertexClaude | Family::FoundryClaude
    ) {
        config.providers[0].wire_profile = Some(
            serde_json::from_value(json!({
                "provider_id":provider,"profile_name":"fixture","base_url":endpoint,
                "protocol":protocol,"auth":"none","models":[{
                    "display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,
                    "metadata":{"applicationOutputTokens":4096}}]
            }))
            .expect("explicit fixture output budget"),
        );
    }
    let mut runtime = ModelRuntime::from_config(config).expect("provider route");
    if matches!(family, Family::GeminiInteractions) {
        runtime =
            runtime.with_credential_provider(Arc::new(llm_runtime::StaticCredentialProvider::new(
                llm_runtime::Credential::ApiKey("fake-mcp-provider-key".into()),
            )));
    }
    Arc::new(
        ApiService::new_with_routing(
            Arc::new(runtime),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "mcp-multi-provider-322",
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

async fn peer(
    response_tx: tokio::sync::mpsc::Sender<Bytes>,
    mut request_rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> Vec<Value> {
    let mut requests = Vec::new();
    for method in ["initialize", "tools/list", "tools/call"] {
        let request = loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv())
                .await
                .expect("MCP request arrives")
                .expect("MCP peer remains connected");
            let request: Value = serde_json::from_slice(&frame).expect("MCP request JSON");
            if request.get("id").is_some() {
                break request;
            }
        };
        assert_eq!(request["method"], method);
        let result = match method {
            "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},
                "serverInfo":{"name":"multi-provider-fixture","version":"1"}}),
            "tools/list" => {
                json!({"tools":[{"name":"echo","description":"Echo exact provider arguments",
                "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]})
            }
            "tools/call" => {
                assert_eq!(request["params"]["name"], "echo");
                assert_eq!(request["params"]["arguments"], json!({"text":ARGUMENT}));
                Value::Null
            }
            _ => unreachable!(),
        };
        // Keep the lone surrogate in structuredContent's source, rather than
        // attempting to lower it into a serde_json::String.
        let result = if method == "tools/call" {
            format!(
                r#"{{"content":[{{"type":"text","text":"fallback"}}],"structuredContent":{RESULT}}}"#
            )
        } else {
            result.to_string()
        };
        let response = format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
            request["id"]
        );
        response_tx
            .send(Bytes::from(format!("{response}\n")))
            .await
            .unwrap();
        requests.push(request);
    }
    requests
}

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(text) => text == expected || text.starts_with(&format!("{expected}\n")),
        Value::Array(items) => items.iter().any(|item| contains_string(item, expected)),
        Value::Object(fields) => fields.values().any(|item| contains_string(item, expected)),
        _ => false,
    }
}

async fn roundtrip(family: Family) {
    let (connection, response_tx, request_rx) = paired_mcp_connection();
    let peer = tokio::spawn(peer(response_tx, request_rx));
    let client = Arc::new(McpClient::new("multi_provider", std::env::temp_dir(), connection).await);
    client.initialize().await.expect("MCP initialize");
    let tool = client.list_tools().await.expect("MCP discovery").remove(0);
    let registry = Arc::new(McpRegistry::new(Arc::new(UnusedMcpTransport)));
    registry.register_client("multi_provider", client).await;
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let mut ctx = tool_api::test_support::ctx_for_file_tools(
        fs,
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![temp.path().to_path_buf()],
    );
    ctx.mcp_registry = Some(registry.clone());
    let mcp_tool = Arc::new(MCPTool::new_for_tool(
        ctx,
        tool.full_name,
        tool.description,
        tool.input_schema,
        tool.output_schema,
        None,
        tool.search_hint,
        true,
        tool.requires_user_interaction,
    )) as Arc<dyn Tool>;
    let tools = Arc::new(ToolRegistry::new());
    tools.register_mcp_tools(McpConnectionId::new(), vec![mcp_tool]);
    let transport = Arc::new(ProviderFixture {
        responses: Mutex::new(VecDeque::from(family.responses())),
        requests: Mutex::new(Vec::new()),
        request_shapes: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(service(family, transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.into(),
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
            temp.path().to_path_buf(),
        )
        .with_mcp_registry(registry),
    );
    let result = orchestrator
        .run_turn("Run echo once and finish.")
        .await
        .unwrap();
    let request_shapes = transport.request_shapes.lock().unwrap().clone();
    let output_events = output.snapshot().await;
    assert!(
        matches!(result, ConversationOutcome::EndTurn { turn_count: 2, .. }),
        "{family:?}: {result:?}; requests={request_shapes:?}; output={output_events:?}"
    );
    let mcp_requests = peer.await.unwrap();
    assert_eq!(mcp_requests.len(), 3);
    // A SDK-only handshake override must never remove ordinary MCP capabilities.
    assert_eq!(
        mcp_requests[0]["params"]["capabilities"]["roots"]["listChanged"],
        true
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "{family:?}");
    assert!(transport.responses.lock().unwrap().is_empty());
    assert!(
        output_events.iter().any(|event| matches!(event,
        OutputEvent::Text {text} if text.string_at("") == Some("done"))),
        "{family:?}"
    );
    assert!(
        contains_string(&requests[0]["tools"], TOOL),
        "{family:?} advertises MCP tool"
    );
    assert!(
        contains_string(&requests[1], RESULT),
        "{family:?} preserves exact MCP result: {}",
        requests[1]
    );
    match family {
        Family::Anthropic
        | Family::BedrockClaude
        | Family::VertexClaude
        | Family::FoundryClaude => assert!(contains_string(&requests[1]["messages"], CALL_ID)),
        Family::OpenAiChat | Family::AzureOpenAi => assert!(requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == CALL_ID)),
        Family::OpenAiResponses => assert!(requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == CALL_ID)),
        Family::Gemini | Family::VertexGemini => {
            assert!(requests[1]["contents"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |message| message["parts"].as_array().unwrap().iter().any(|part| part
                        .pointer("/functionResponse/name")
                        .and_then(Value::as_str)
                        == Some(TOOL))
                ))
        }
        Family::GeminiInteractions => assert!(requests[1]["input"]["steps"]
            .as_array()
            .expect("interaction steps")
            .iter()
            .any(|item| item["type"] == "function_result" && item["call_id"] == CALL_ID)),
    }
}

#[tokio::test]
async fn mcp_322_roundtrip_anthropic() {
    roundtrip(Family::Anthropic).await;
}

#[tokio::test]
async fn mcp_322_roundtrip_openai_chat() {
    roundtrip(Family::OpenAiChat).await;
}

#[tokio::test]
async fn mcp_322_roundtrip_openai_responses() {
    roundtrip(Family::OpenAiResponses).await;
}

#[tokio::test]
async fn mcp_322_roundtrip_gemini() {
    roundtrip(Family::Gemini).await;
}

macro_rules! hosted_roundtrip {
    ($test:ident, $family:ident) => {
        #[tokio::test]
        async fn $test() {
            roundtrip(Family::$family).await;
        }
    };
}
hosted_roundtrip!(mcp_322_roundtrip_bedrock_claude, BedrockClaude);
hosted_roundtrip!(mcp_322_roundtrip_vertex_claude, VertexClaude);
hosted_roundtrip!(mcp_322_roundtrip_foundry_claude, FoundryClaude);
hosted_roundtrip!(mcp_322_roundtrip_azure_openai, AzureOpenAi);
hosted_roundtrip!(mcp_322_roundtrip_gemini_interactions, GeminiInteractions);
hosted_roundtrip!(mcp_322_roundtrip_vertex_gemini, VertexGemini);
