//! Carry Native 2.1.295's three `$.tool.register` `isDeferred` states through
//! the production ModHost, ModSessionContext, ToolRegistry, and first SDK tool
//! request. The public tool-register request fixture remains 2.1.293; this is
//! a candidate integration check for the 2.1.295 source-defined behavior.

use async_trait::async_trait;
use hooks::registry::HookRegistry;
use lingxi_core::host::OutputEvent;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;
use tool_api::tool_search_view::ToolRegistryView;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};

const MODEL: &str = "claude-sonnet-4-5";
const PLUGIN: &str = "current-alignment-mod-tool-wire-295";
const TOOL_OMITTED: &str = "mcp__current-alignment-mod-tool-wire-295__deferred_omitted";
const TOOL_ALWAYS_LOAD: &str = "mcp__current-alignment-mod-tool-wire-295__always_load";
const TOOL_TRUE: &str = "mcp__current-alignment-mod-tool-wire-295__deferred_true";
const MODULE_SOURCE: &str = include_str!("fixtures/current_alignment_mod_tool_wire_295.js");
const FIXED_END_TURN_SSE: &[u8] = br#"event: message_start
data: {"type":"message_start","message":{"id":"msg_mod_tool_wire_295","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fixture complete"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}

event: message_stop
data: {"type":"message_stop"}

"#;

struct CapturingProviderTransport {
    request_bodies: Mutex<Vec<Vec<u8>>>,
    calls: AtomicUsize,
}

#[async_trait]
impl Transport for CapturingProviderTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.request_bodies
            .lock()
            .expect("provider request capture lock")
            .push(request.body.to_vec());
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: FIXED_END_TURN_SSE.to_vec().into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<CapturingProviderTransport>) -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(serde_json::json!({
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
    .expect("fixed first-party Anthropic route config");

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "current-alignment-mod-tool-wire-295",
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

/// `build_wire_tools` uses the advertised ToolSearch name to exercise the
/// existing request deferral gate. This inert built-in is only that name gate:
/// the fixed model reply ends the turn without invoking it, and it does not
/// stand in for any of the production Mod registrations under test.
struct ToolSearchGateMarker {
    schema: Utf16JsonProjection,
}

#[async_trait]
impl Tool for ToolSearchGateMarker {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1_024
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "unused request-gate fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Deferred tool search gate fixture".into()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        "Deferred tool search gate fixture".into()
    }

    async fn call(
        &self,
        _input: Utf16JsonProjection,
        _ctx: tool_api::context::ToolUseContext,
        _tx: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

fn registration_module(root: &Path) -> std::path::PathBuf {
    let module = root.join("register.js");
    std::fs::write(&module, MODULE_SOURCE).expect("write self-contained Mod fixture");
    module
}

fn expected_schema(state: &str) -> Utf16JsonProjection {
    let json = format!(
        r#"{{"type":"object","properties":{{"state":{{"type":"string","const":"{state}","description":"source-carried state for {state}"}},"value":{{"type":"integer","minimum":7}}}},"required":["state","value"],"additionalProperties":false}}"#
    );
    Utf16JsonProjection::parse(&json).expect("fixed source schema is representable")
}

#[tokio::test]
async fn native_295_mod_tool_register_states_reach_first_sdk_tools_array() {
    let temp = tempdir().expect("isolated Mod and session root");
    let module = registration_module(temp.path());
    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start production Mod worker");
    host.load(
        PLUGIN,
        temp.path(),
        &module,
        Utf16JsonProjection::parse("{}").expect("empty Mod options"),
    )
    .await
    .expect("load three-registration session.start fixture");

    let mut tool_registry = ToolRegistry::new();
    tool_registry.set_deferral(Arc::new(tool_api::DeferralState::new(
        tool_api::ToolSearchMode::Enabled,
        false,
    )));
    tool_registry.register_builtin(Arc::new(ToolSearchGateMarker {
        schema: Utf16JsonProjection::parse(
            r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}"#,
        )
        .expect("ToolSearch gate schema"),
    }));
    let tool_registry = Arc::new(tool_registry);
    let deferred_view = tool_registry.tool_search_view();

    let transport = Arc::new(CapturingProviderTransport {
        request_bodies: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let mut hook_registry = HookRegistry::new();
    hook_registry.set_mod_host(host);
    let hook_registry = Arc::new(tokio::sync::RwLock::new(hook_registry));
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 1,
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            tool_registry.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            temp.path().to_path_buf(),
        )
        .with_hook_registry(hook_registry),
    );

    // The host's real SessionStart Mod dispatch supplies ConversationOrchestrator
    // as ModSessionContext. Each $.tool.register therefore inserts into the
    // same ToolRegistry used by request assembly below.
    orchestrator.fire_session_start("startup").await;

    let registration_log = output
        .snapshot()
        .await
        .into_iter()
        .find_map(|event| match event {
            OutputEvent::ModLog { plugin, text } if plugin == PLUGIN => {
                text.string_at("").map(str::to_owned)
            }
            _ => None,
        })
        .expect("Mod logged all three production registration receipts");
    assert_eq!(
        registration_log,
        format!(
            r#"[{{"tool":"{TOOL_OMITTED}"}},{{"tool":"{TOOL_ALWAYS_LOAD}"}},{{"tool":"{TOOL_TRUE}"}}]"#
        )
    );

    let catalog = tool_registry.available_tools(&ToolStaticContext::default());
    let registered_names = catalog
        .iter()
        .filter(|tool| {
            tool.name()
                .starts_with("mcp__current-alignment-mod-tool-wire-295__")
        })
        .map(|tool| tool.name().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        registered_names,
        [TOOL_ALWAYS_LOAD, TOOL_OMITTED, TOOL_TRUE],
        "the production Mod catalog keeps all three names in provider order"
    );
    for (name, should_defer, always_load) in [
        (TOOL_OMITTED, true, false),
        (TOOL_ALWAYS_LOAD, false, true),
        (TOOL_TRUE, true, false),
    ] {
        let tool = catalog
            .iter()
            .find(|tool| tool.name() == name)
            .expect("registered candidate is in the live catalog");
        assert_eq!(tool.should_defer(), should_defer, "{name}");
        assert_eq!(tool.always_load(), always_load, "{name}");
        assert_eq!(
            tool.input_schema(),
            &expected_schema(match name {
                TOOL_OMITTED => "deferred_omitted",
                TOOL_ALWAYS_LOAD => "always_load",
                TOOL_TRUE => "deferred_true",
                _ => unreachable!("test candidates are fixed above"),
            }),
            "the registration's inputSchema carrier remains in the production catalog: {name}"
        );
    }
    let deferred_names = deferred_view
        .entries()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(deferred_names, [TOOL_OMITTED, TOOL_TRUE]);

    let outcome = orchestrator
        .run_turn_streaming("Report the fixed registration fixture result.")
        .await
        .expect("fixed local Anthropic SSE response ends the first turn");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

    let request_bodies = transport
        .request_bodies
        .lock()
        .expect("captured request lock")
        .clone();
    assert_eq!(
        request_bodies.len(),
        1,
        "only the first SDK request is captured"
    );
    let request = Utf16JsonProjection::parse(
        std::str::from_utf8(&request_bodies[0]).expect("SDK request is valid UTF-8 JSON"),
    )
    .expect("parse exact captured SDK body without reserializing it");
    assert_eq!(request.string_at("/model"), Some(MODEL));
    let tool_count = request
        .array_len("/tools")
        .expect("first request tools array");
    let request_tool_names = (0..tool_count)
        .map(|index| {
            request
                .string_at(&format!("/tools/{index}/name"))
                .expect("provider tool name")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        request_tool_names,
        ["ToolSearch", TOOL_ALWAYS_LOAD],
        "omitted/true candidates stay deferred while false is always present on the first request"
    );

    let expected_schema = expected_schema("always_load");
    let request_schema = request
        .subprojection("/tools/1/input_schema")
        .expect("Anthropic SDK emitted the registered schema carrier");
    assert_eq!(request_schema, expected_schema);
    assert_eq!(
        request.string_at("/tools/1/description"),
        Some("2.1.295 false isDeferred candidate")
    );

    println!(
        "CURRENT_ALIGNMENT_MOD_TOOL_WIRE_295={}",
        serde_json::json!({
            "registered_tool_names": registered_names,
            "deferred_search_catalog": deferred_names,
            "first_sdk_request_tool_names": request_tool_names,
            "always_load_schema_state": request_schema.string_at("/properties/state/const"),
            "tool_search": "name-only gate marker; no search invocation",
            "captured_first_party_requests": transport.calls.load(Ordering::SeqCst),
            "native_public_request_fixture": "2.1.293; source binding is 2.1.295"
        })
    );
}
