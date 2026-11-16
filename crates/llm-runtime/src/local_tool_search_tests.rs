//! Native local discovery keeps frozen receipts out of the top-level prefix.
use super::*;
use crate::{
    ClientConfig, ContentBlock as HistoryBlock, Credential, HistoryEvent, ModelRuntime,
    StaticCredentialProvider,
};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, MessageId, ToolUseId};
use lingxi_llm_client::{
    codecs::openai::responses::OpenAiResponsesCodec,
    protocol::{
        CacheBreakpoint, CachePosition, CacheTtl, ContentBlock, ConversationMessage, MessageRole,
        ToolSpec,
    },
};
use serde_json::{json, Value};
use std::{borrow::Cow, collections::HashSet, sync::Arc};

const MODEL: &str = "gpt-6.1-sol";
const NAME: &str = "mcp__fixture__lookup";
const CALL_ID: &str = "tsc_frozen_fixture";
const KEY: &str = "synthetic-local-tool-search-key";

fn payload(value: Value) -> JsonPayload {
    JsonPayload::from_generated_value(&value).unwrap()
}

fn native_call() -> JsonPayload {
    payload(json!({"type":"tool_search_call","execution":"client",
        "call_id":CALL_ID,"status":"completed","arguments":{"query":"lookup fixture"}}))
}

fn definition() -> JsonPayload {
    payload(
        json!({"name":NAME,"description":"frozen tool definition v1",
        "input_schema":{"type":"object","properties":{"q":{"type":"string","description":"frozen schema v1"}},"required":["q"],"additionalProperties":false},
        "strict":false,"defer_loading":true}),
    )
}

fn history(with_snapshot: bool) -> History {
    History {
        calls: BTreeMap::from([(CALL_ID.into(), native_call())]),
        definitions: if with_snapshot {
            BTreeMap::from([(
                CALL_ID.into(),
                JsonPayload::array_from_items(
                    &[definition()],
                    lingxi_llm_client::exact_json::JsonEncoding::JavaScript,
                )
                .unwrap(),
            )])
        } else {
            BTreeMap::new()
        },
        legacy_names: BTreeSet::new(),
    }
}

fn search_tool() -> ToolSpec {
    serde_json::from_value(json!({"name":"ToolSearch","description":"Find an authorized deferred tool",
        "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}})).unwrap()
}

fn eager_tool() -> ToolSpec {
    serde_json::from_value(json!({"name":"Read","description":"Read a fixture",
        "input_schema":{"type":"object","properties":{}},"strict":false}))
    .unwrap()
}

fn deferred_tool() -> ToolSpec {
    serde_json::from_str(definition().as_json_source()).unwrap()
}

fn profile(endpoint: &str, model: &str, provider: &str, protocol: &str) -> wire::ProviderProfile {
    serde_json::from_value(json!({"provider_id":provider,"profile_name":"local-search",
        "base_url":endpoint,"protocol":protocol,"auth":"none",
        "models":[{"display_model":model,"request_model":model,"billing_model":model}]}))
    .unwrap()
}

fn official() -> wire::ProviderProfile {
    profile(
        "https://api.openai.com/v1",
        MODEL,
        "openai",
        "open_ai_responses",
    )
}

fn request(stream: bool, discovered: bool, with_snapshot: bool) -> LlmRequest {
    let mut request = LlmRequest::new(MODEL).with_profile("local-search");
    request.stream = stream;
    request.input.max_tokens = Some(128);
    request.input.tools = vec![eager_tool(), search_tool()];
    // Central ApiService captures an admitted (possibly empty) history before
    // selected-route projection. Merely naming a caller tool ToolSearch is not
    // authority to replace it with the host's native discovery protocol.
    request.execution.local_tool_search_history = Some(Arc::new(History::default()));
    request
        .input
        .messages
        .push(wire::ConversationMessage::user_text(
            "Find the fixture tool",
        ));
    if discovered {
        request.input.tools.push(deferred_tool());
        request
            .input
            .messages
            .push(ConversationMessage::assistant(vec![
                serde_json::from_value(json!({"type":"tool_use","id":CALL_ID,
                "name":"ToolSearch","input":{"query":"lookup fixture"}}))
                .unwrap(),
            ]));
        request.input.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: vec![serde_json::from_value(json!({"type":"tool_result",
                "tool_use_id":CALL_ID,"content":"frozen search display",
                "blocks":[{"type":"tool_reference","tool_name":NAME}]}))
            .unwrap()],
            native_options: Vec::new(),
        });
        request.execution.local_tool_search_history = Some(Arc::new(history(with_snapshot)));
    }
    request.execution.input_protocol = Some(wire::ProtocolFamily::OpenAiResponses);
    request
}

fn fresh_runtime(endpoint: &str, model: &str) -> ModelRuntime {
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"open_ai","profile_name":"local-search","base_url":endpoint,
        "protocol":"open_ai_responses","auth":"api_key",
        "credential":{"type":"host_managed","id":"local-tool-search-fixture"},
        "models":[{"display_model":MODEL,"request_model":model,"billing_model":model,
            "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":true}}]
    }]})).unwrap();
    ModelRuntime::from_config(config)
        .unwrap()
        .with_credential_provider(Arc::new(StaticCredentialProvider::new(Credential::ApiKey(
            KEY.into(),
        ))))
}

async fn prepared(runtime: &ModelRuntime, request: &LlmRequest) -> (Value, bool) {
    let call = runtime.prepare(request).await.unwrap();
    let body = serde_json::from_slice(&call.provider_request.wire_body_bytes().unwrap()).unwrap();
    (body, call.local_tool_search_admitted)
}

fn native_item<'a>(body: &'a Value, kind: &str) -> &'a Value {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == kind)
        .unwrap_or_else(|| panic!("{kind} item missing from prepared request"))
}

async fn raw_receipts(runtime: &ModelRuntime, request: &LlmRequest) -> Vec<String> {
    let call = runtime.prepare(request).await.unwrap();
    let source = String::from_utf8(call.provider_request.wire_body_bytes().unwrap()).unwrap();
    let body = JsonPayload::parse(&source).unwrap();
    (0..body.array_len("/input").unwrap())
        .filter_map(|index| {
            let item = body.array_item_at("/input", index)?;
            matches!(
                item.string_at("/type").as_deref(),
                Some("tool_search_call" | "tool_search_output")
            )
            .then(|| item.as_json_source().to_owned())
        })
        .collect()
}

#[tokio::test]
async fn complete_and_stream_discovery_keeps_top_level_tools_and_frozen_schema_stable() {
    for stream in [false, true] {
        let runtime = fresh_runtime("https://api.openai.com/v1", MODEL);
        let initial = request(stream, false, false);
        let discovered = request(stream, true, true);
        let canonical = discovered.input.clone();
        let (initial_body, admitted) = prepared(&runtime, &initial).await;
        let (loaded_body, loaded_admitted) = prepared(&runtime, &discovered).await;
        assert!(admitted && loaded_admitted);
        assert_eq!(
            initial_body["tools"], loaded_body["tools"],
            "discovery cannot grow the top-level prompt prefix"
        );
        assert_eq!(initial_body["tools"].as_array().unwrap().len(), 2);
        let search = initial_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["type"] == "tool_search")
            .unwrap();
        assert_eq!(
            search["parameters"],
            serde_json::from_str::<Value>(search_tool().input_schema.as_json_source()).unwrap(),
            "eager tools must not consume the native search schema overlay"
        );
        assert!(initial_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["type"] == "tool_search" && tool["execution"] == "client"));
        assert_eq!(
            native_item(&loaded_body, "tool_search_call")["call_id"],
            CALL_ID
        );
        let published = native_item(&loaded_body, "tool_search_output");
        assert_eq!(published["call_id"], CALL_ID);
        assert_eq!(published["tools"][0]["name"], NAME);
        assert_eq!(
            published["tools"][0]["parameters"]["properties"]["q"]["description"],
            "frozen schema v1"
        );
        assert_eq!(published["tools"][0]["defer_loading"], true);
        assert_eq!(discovered.input, canonical);

        let mut updated_live_catalog = discovered.clone();
        let tool = updated_live_catalog
            .input
            .tools
            .iter_mut()
            .find(|tool| tool.name == NAME)
            .unwrap();
        tool.description = JsonPayload::string("mutable catalog v2");
        tool.input_schema =
            payload(json!({"type":"object","properties":{"changed":{"type":"integer"}}}));
        let (replayed, _) = prepared(&runtime, &updated_live_catalog).await;
        assert_eq!(loaded_body["tools"], replayed["tools"]);
        assert_eq!(
            native_item(&loaded_body, "tool_search_call"),
            native_item(&replayed, "tool_search_call")
        );
        assert_eq!(
            native_item(&loaded_body, "tool_search_output"),
            native_item(&replayed, "tool_search_output"),
            "history uses the frozen schema, never a later live catalog"
        );
        assert_eq!(
            raw_receipts(&runtime, &discovered).await,
            raw_receipts(&runtime, &updated_live_catalog).await,
            "native call and frozen schema source bytes stay identical across catalog updates"
        );
    }
}

#[tokio::test]
async fn dropped_discovery_preparation_and_cold_rebuild_keep_identical_prefixes() {
    for stream in [false, true] {
        let runtime = fresh_runtime("https://api.openai.com/v1", MODEL);
        let request = request(stream, true, true);
        let original = request.input.clone();
        let pending = runtime.prepare(&request).await.unwrap();
        let before_drop = pending.provider_request.wire_body_bytes().unwrap();
        drop(pending);
        let (resumed, _) = prepared(&runtime, &request).await;
        let mut rebuilt = request.clone();
        rebuilt.input =
            serde_json::from_str(&serde_json::to_string(&request.input).unwrap()).unwrap();
        // Re-extract immutable host receipts instead of reusing projected state.
        rebuilt.execution.local_tool_search_history = Some(Arc::new(history(true)));
        let (cold, _) =
            prepared(&fresh_runtime("https://api.openai.com/v1", MODEL), &rebuilt).await;
        assert_eq!(
            serde_json::from_slice::<Value>(&before_drop).unwrap(),
            resumed
        );
        assert_eq!(resumed, cold);
        assert_eq!(request.input, original);
        assert!(
            !request.execution.local_tool_search_admitted,
            "admission is selected-route local"
        );
    }
}

#[test]
fn native_result_expansion_remaps_utf16_breakpoints_and_provenance() {
    let mut request = request(false, true, true);
    let result_units = vec!['r' as u16, 0xd800, 'x' as u16];
    let following_units = vec!['n' as u16, 0xdfff, 'z' as u16];
    if let ContentBlock::ToolResult { content, .. } = &mut request.input.messages[2].content[0] {
        *content = JsonPayload::string_from_utf16_code_units(&result_units);
    }
    request.input.messages[2].content.push(ContentBlock::Text {
        text: String::from_utf16_lossy(&following_units),
        thought_signature: None,
        citations: None,
    });
    request.execution.message_json_string_overrides = BTreeMap::from([
        ("/messages/2/content/0/content".into(), result_units.clone()),
        ("/messages/2/content/1/text".into(), following_units.clone()),
    ]);
    let result_source = MessageId::new();
    let following_source = MessageId::new();
    request.execution.request_content_source_ids = vec![
        vec![HashSet::from([MessageId::new()])],
        vec![HashSet::from([MessageId::new()])],
        vec![
            HashSet::from([result_source]),
            HashSet::from([following_source]),
        ],
    ];
    request
        .input
        .prompt_cache
        .breakpoints
        .push(CacheBreakpoint {
            position: CachePosition::Message { index: 2, block: 1 },
            ttl: CacheTtl::ThirtyMinutes,
            scope: None,
        });
    let original = request.clone();
    let projected = project(&request, &official(), MODEL, AuthStrategy::ApiKey).unwrap();
    assert!(matches!(projected, Cow::Owned(_)));
    assert_eq!(projected.input.messages[2].content.len(), 3);
    assert_eq!(
        projected.execution.message_json_string_overrides["/messages/2/content/1/text"],
        result_units
    );
    assert_eq!(
        projected.execution.message_json_string_overrides["/messages/2/content/2/text"],
        following_units
    );
    assert_eq!(
        projected.input.prompt_cache.breakpoints[0].position,
        CachePosition::Message { index: 2, block: 2 }
    );
    assert_eq!(
        projected.execution.request_content_source_ids[2],
        vec![
            HashSet::from([result_source]),
            HashSet::from([result_source]),
            HashSet::from([following_source])
        ]
    );
    assert_eq!(request.input, original.input);
    assert_eq!(
        request.execution.message_json_string_overrides,
        original.execution.message_json_string_overrides
    );
}

#[tokio::test]
async fn missing_snapshot_publishes_no_callable_schema_and_keeps_exact_retry_context() {
    for stream in [false, true] {
        let runtime = fresh_runtime("https://api.openai.com/v1", MODEL);
        let mut request = request(stream, true, false);
        let units = vec!['r' as u16, 0xd800, 'x' as u16];
        let following_units = vec!['n' as u16, 0xdfff, 'z' as u16];
        if let ContentBlock::ToolResult { content, .. } = &mut request.input.messages[2].content[0]
        {
            *content = JsonPayload::string_from_utf16_code_units(&units);
        }
        request.input.messages[2].content.push(ContentBlock::Text {
            text: String::from_utf16_lossy(&following_units),
            thought_signature: None,
            citations: None,
        });
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/2/content/0/content".into(), units.clone());
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/2/content/1/text".into(), following_units.clone());
        let call = runtime.prepare(&request).await.unwrap();
        let source = String::from_utf8(call.provider_request.wire_body_bytes().unwrap()).unwrap();
        let payload = JsonPayload::parse(&source).unwrap();
        let mut outputs = 0;
        let mut retry_texts = 0;
        let mut following_texts = 0;
        for index in 0..payload.array_len("/input").unwrap() {
            let item = payload.array_item_at("/input", index).unwrap();
            if item.string_at("/type").as_deref() == Some("tool_search_output") {
                assert_eq!(item.array_len("/tools"), Some(0));
                outputs += 1;
            }
            for block in 0..item.array_len("/content").unwrap_or(0) {
                if let Some(text) = item.payload_at(&format!("/content/{block}/text")) {
                    if let Some(actual) = text.root_string_code_units() {
                        if actual == following_units {
                            following_texts += 1;
                        }
                        if actual.starts_with(&units) {
                            assert!(String::from_utf16_lossy(&actual).contains("Retry ToolSearch"));
                            retry_texts += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(outputs, 1);
        assert_eq!(
            following_texts, 1,
            "following exact text moves past the expanded result"
        );
        assert_eq!(
            retry_texts, 1,
            "the exact original result and retry hint are both retained"
        );
    }
}

#[test]
fn capture_history_freezes_receipts_strips_carriers_and_preserves_legacy_loading() {
    let use_id = ToolUseId::new();
    let call_receipt = Utf16JsonProjection::parse(&format!(
        r#"{{"type":"{CALL}","call":{}}}"#,
        native_call().as_json_source()
    ))
    .unwrap();
    let definition_receipt = Utf16JsonProjection::parse(&format!(
        r#"{{"type":"{DISCOVERY}","call_id":"{CALL_ID}","definitions":[{}]}}"#,
        definition().as_json_source()
    ))
    .unwrap();
    let assistant = HostMessage::Assistant {
        id: MessageId::new(),
        content: vec![
            HostBlock::ToolUse {
                id: use_id.clone(),
                name: "ToolSearch".into(),
                input: Utf16JsonProjection::parse(r#"{"query":"fixture"}"#).unwrap(),
                provider_id: Some(CALL_ID.into()),
            },
            HostBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: call_receipt,
            },
        ],
        stop_reason: Some("tool_use".into()),
        resumable: None,
        per_turn_effort: None,
    };
    let mut result = HostMessage::user(MessageId::new(), String::new());
    if let HostMessage::User { content, .. } = &mut result {
        *content = vec![
            HostBlock::ToolResult {
                tool_use_id: use_id,
                content: "display".into(),
                is_error: None,
                provider_tool_use_id: Some(CALL_ID.into()),
                content_blocks: Some(
                    Utf16JsonProjection::parse(&format!(
                        r#"[{{"type":"tool_reference","tool_name":"{NAME}"}}]"#
                    ))
                    .unwrap(),
                ),
            },
            HostBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: definition_receipt,
            },
        ];
    }
    let mut messages = vec![assistant, result];
    let durable = serde_json::to_string(&messages).unwrap();
    let captured = capture_history(&mut messages).unwrap();
    assert_eq!(captured.calls[CALL_ID], native_call());
    assert_eq!(
        captured.definitions[CALL_ID].array_item(0).unwrap(),
        definition()
    );
    assert!(captured.legacy_names.is_empty());
    assert!(messages.iter().all(|message| match message {
        HostMessage::Assistant { content, .. } | HostMessage::User { content, .. } => content
            .iter()
            .all(|block| !matches!(block, HostBlock::ProviderContent { .. })),
        _ => true,
    }));
    let mut restored: Vec<HostMessage> = serde_json::from_str(&durable).unwrap();
    assert_eq!(
        capture_history(&mut restored).unwrap(),
        captured,
        "cold restore re-extracts identical frozen receipts"
    );
    // With carriers removed, this is the legacy function-call history. Its
    // loaded schema stays in the current top-level catalog for safe replay.
    let legacy = capture_history(&mut messages).unwrap();
    assert!(legacy.legacy_names.contains(NAME));
    let mut request = request(false, true, true);
    request.execution.local_tool_search_history = Some(Arc::new(legacy));
    assert!(tools(&request, &official(), MODEL, AuthStrategy::ApiKey)
        .unwrap()
        .0
        .iter()
        .any(|tool| tool.name == NAME));
}

#[test]
fn route_and_auth_gates_do_not_native_project_unsupported_or_caller_owned_search() {
    let request = request(false, true, true);
    let mut unadmitted = request.clone();
    unadmitted.execution.local_tool_search_history = None;
    assert!(matches!(
        project(&unadmitted, &official(), MODEL, AuthStrategy::ApiKey).unwrap(),
        Cow::Borrowed(_)
    ));
    for (profile, model, auth) in [
        (
            profile(
                "https://compatible.test/v1",
                MODEL,
                "openai",
                "open_ai_responses",
            ),
            MODEL,
            AuthStrategy::ApiKey,
        ),
        (
            profile(
                "https://api.openai.com/v1",
                "gpt-5.3",
                "openai",
                "open_ai_responses",
            ),
            "gpt-5.3",
            AuthStrategy::ApiKey,
        ),
        (
            profile(
                "https://chatgpt.com/backend-api/codex",
                MODEL,
                "openai",
                "open_ai_responses",
            ),
            MODEL,
            AuthStrategy::OAuthBearer,
        ),
        (
            profile(
                "https://api.anthropic.com",
                "claude-sonnet-4-6",
                "anthropic",
                "anthropic_messages",
            ),
            "claude-sonnet-4-6",
            AuthStrategy::ApiKey,
        ),
        (official(), MODEL, AuthStrategy::None),
    ] {
        assert!(matches!(
            project(&request, &profile, model, auth).unwrap(),
            Cow::Borrowed(_)
        ));
    }
    let mut caller_owned = request.clone();
    caller_owned
        .input
        .set_hosted_openai_tool_search(Some(OpenAiToolSearchConfig {
            execution: OpenAiToolSearchExecution::Client,
            description: Some("caller owned search".into()),
            parameters: Some(payload(json!({"type":"object"}))),
        }));
    assert!(matches!(
        project(&caller_owned, &official(), MODEL, AuthStrategy::ApiKey).unwrap(),
        Cow::Borrowed(_)
    ));
    let mut forced = request.clone();
    forced.input.tool_choice = wire::ToolChoice::Tool {
        name: "Read".into(),
    };
    assert!(matches!(
        project(&forced, &official(), MODEL, AuthStrategy::ApiKey).unwrap(),
        Cow::Borrowed(_)
    ));
    forced.input.tool_choice = wire::ToolChoice::Any;
    assert!(
        matches!(
            project(&forced, &official(), MODEL, AuthStrategy::ApiKey).unwrap(),
            Cow::Borrowed(_)
        ),
        "required tool choice must not become automatic on a hosted-only projection"
    );
    assert!(!request.execution.local_tool_search_admitted);
}

#[test]
fn prompt_fingerprint_uses_selected_wire_model_and_stable_native_tool_prefix() {
    let mut initial = request(false, false, false);
    let mut discovered = request(false, true, true);
    let mut selected = official();
    selected.auth = AuthStrategy::ApiKey;
    for request in [&mut initial, &mut discovered] {
        request.input.model = "local-display-alias".into();
        request.execution.prompt_projection_profile = Some(selected.clone());
        request.execution.context_usage_protocol = Some(wire::ProtocolFamily::OpenAiResponses);
        request.execution.context_usage_wire_model = Some(MODEL.into());
    }
    let prefix = crate::context_usage::ContextUsagePrefix::capture(&initial, &[]).unwrap();
    assert!(
        prefix.matches_prompt(&discovered),
        "discovered schemas change history rather than the physical top-level tool prefix"
    );
    discovered
        .input
        .tools
        .iter_mut()
        .find(|tool| tool.name == NAME)
        .unwrap()
        .description = JsonPayload::string("live catalog v2");
    assert!(
        prefix.matches_prompt(&discovered),
        "historical native definitions are not replaced by live catalog edits"
    );
    discovered.input.tools[0].description = JsonPayload::string("changed eager tool contract");
    assert!(
        !prefix.matches_prompt(&discovered),
        "a real change to an eager tool must invalidate the prompt fingerprint"
    );
}

#[tokio::test]
async fn complete_and_stream_unsupported_routes_keep_portable_function_search() {
    for stream in [false, true] {
        for (endpoint, model) in [
            ("https://compatible.test/v1", MODEL),
            ("https://api.openai.com/v1", "gpt-5.3"),
        ] {
            let runtime = fresh_runtime(endpoint, model);
            let request = request(stream, true, true);
            let canonical = request.input.clone();
            let (body, admitted) = prepared(&runtime, &request).await;
            assert!(!admitted);
            assert!(body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "function" && tool["name"] == "ToolSearch"));
            assert!(body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "function" && tool["name"] == NAME));
            assert!(body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("defer_loading").is_none()));
            assert!(body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call" && item["name"] == "ToolSearch"));
            assert!(body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "tool_search_call"
                    && item["type"] != "tool_search_output"));
            assert_eq!(request.input, canonical);
        }
    }
}

#[tokio::test]
async fn exact_surrogate_search_description_keeps_portable_wire_in_both_modes() {
    for stream in [false, true] {
        let runtime = fresh_runtime("https://api.openai.com/v1", MODEL);
        let mut request = request(stream, false, false);
        let units = vec!['d' as u16, 0xd800, 's' as u16];
        request
            .input
            .tools
            .iter_mut()
            .find(|tool| tool.name == "ToolSearch")
            .unwrap()
            .description = JsonPayload::string_from_utf16_code_units(&units);
        let canonical = request.input.clone();
        let prepared = runtime.prepare(&request).await.unwrap();
        assert!(
            !prepared.local_tool_search_admitted,
            "the native descriptor cannot losslessly represent this exact description"
        );
        let source =
            String::from_utf8(prepared.provider_request.wire_body_bytes().unwrap()).unwrap();
        let body = JsonPayload::parse(&source).unwrap();
        let tools = body.payload_at("/tools").unwrap();
        let mut search_descriptions = 0;
        for index in 0..tools.array_len("").unwrap() {
            let tool = tools.array_item(index).unwrap();
            assert_ne!(tool.string_at("/type").as_deref(), Some("tool_search"));
            if tool.string_at("/name").as_deref() == Some("ToolSearch") {
                assert_eq!(
                    tool.payload_at("/description")
                        .unwrap()
                        .root_string_code_units(),
                    Some(units.clone())
                );
                search_descriptions += 1;
            }
        }
        assert_eq!(search_descriptions, 1);
        assert!(source.contains("\\ud800"));
        assert_eq!(request.input, canonical);
    }
}

#[test]
fn native_calls_require_valid_client_receipts_and_admission_for_host_dispatch() {
    let native = native_call();
    assert_eq!(call(&native).unwrap().0, CALL_ID);
    for invalid_call in [
        native
            .with_object_field("execution", &JsonPayload::string("server"))
            .unwrap(),
        native
            .with_object_field("status", &JsonPayload::string("in_progress"))
            .unwrap(),
        native
            .with_object_field("arguments", &JsonPayload::string("not an object"))
            .unwrap(),
        native
            .with_object_field("call_id", &JsonPayload::string("\ninvalid"))
            .unwrap(),
        native
            .with_object_field(
                "call_id",
                &JsonPayload::string_from_utf16_code_units(&[0xd800]),
            )
            .unwrap(),
    ] {
        assert!(call(&invalid_call).is_none());
    }

    for admitted in [false, true] {
        let raw = json!({"id":"response-search","model":MODEL,"status":"completed",
            "output":[serde_json::from_str::<Value>(native.as_json_source()).unwrap()],
            "usage":{"input_tokens":20,"output_tokens":5}});
        let decoded = OpenAiResponsesCodec
            .decode_response(
                &lingxi_llm_client::HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: serde_json::to_vec(&raw).unwrap().into(),
                },
                &CodecContext::new(&official(), MODEL, RequestMode::Complete),
            )
            .unwrap();
        let mut response = crate::history_projection::project_response(
            decoded,
            crate::ProviderResponse::json(200, raw),
            wire::ProtocolFamily::OpenAiResponses,
        )
        .unwrap();
        if admitted {
            crate::history_projection::admit_local_tool_search(&mut response).unwrap();
        }
        assert_eq!(response.content.iter().filter(|block| matches!(block, HistoryBlock::ToolCall { name, .. } if name == "ToolSearch")).count(), usize::from(admitted));

        let mut stream = crate::history_projection::HistoryProjector::projection(
            wire::ProtocolFamily::OpenAiResponses,
            JsonPayload::default(),
        )
        .unwrap();
        stream.local_tool_search_admitted = admitted;
        let events = stream
            .events(vec![
                Ok(wire::StreamEvent::ProviderContent {
                    block: 0,
                    protocol: wire::ProtocolFamily::OpenAiResponses,
                    value: native.clone(),
                }),
                Ok(wire::StreamEvent::End {
                    stop_reason: wire::StopReason::Other("requires_action".into()),
                    usage: Default::default(),
                    inference: Default::default(),
                }),
            ])
            .unwrap();
        assert_eq!(events.iter().filter(|event| matches!(event, HistoryEvent::ContentBlockStart { content_block: HistoryBlock::ToolCall { name, .. }, .. } if name == "ToolSearch")).count(), usize::from(admitted));
        if admitted {
            assert!(events.iter().any(|event| matches!(event, HistoryEvent::MessageDelta { delta, .. } if delta.stop_reason.as_deref() == Some("tool_use"))));
        }
    }
}

fn tool_calls(events: &[HistoryEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(event,
        HistoryEvent::ContentBlockStart { content_block: HistoryBlock::ToolCall { name, .. }, .. }
            if name == "ToolSearch")
        })
        .count()
}

fn receipts(events: &[HistoryEvent]) -> usize {
    events.iter().filter(|event| matches!(event,
        HistoryEvent::ContentBlockStart { content_block: HistoryBlock::ProviderContent { value, .. }, .. }
            if value.string_at("/type").as_deref() == Some(CALL))).count()
}

fn native_projector() -> crate::history_projection::HistoryProjector {
    let mut projector = crate::history_projection::HistoryProjector::projection(
        wire::ProtocolFamily::OpenAiResponses,
        JsonPayload::default(),
    )
    .unwrap();
    projector.local_tool_search_admitted = true;
    projector
}

fn native_event() -> Result<wire::StreamEvent, wire::LlmError> {
    Ok(wire::StreamEvent::ProviderContent {
        block: 0,
        protocol: wire::ProtocolFamily::OpenAiResponses,
        value: native_call(),
    })
}

#[test]
fn native_stream_call_waits_for_success_and_failed_or_canceled_bodies_do_not_dispatch() {
    for stop in [
        wire::StopReason::Other("requires_action".into()),
        wire::StopReason::MaxTokens,
        wire::StopReason::Refusal,
        wire::StopReason::Other("failed".into()),
    ] {
        let successful = stop == wire::StopReason::Other("requires_action".into());
        let mut projector = native_projector();
        let interim = projector
            .events(vec![
                native_event(),
                native_event(),
                Ok(wire::StreamEvent::BlockEnd { block: 0 }),
            ])
            .unwrap();
        assert_eq!(
            tool_calls(&interim),
            0,
            "a completed item alone does not authorize host execution"
        );
        assert_eq!(receipts(&interim), 0);
        let terminal = projector
            .events(vec![Ok(wire::StreamEvent::End {
                stop_reason: stop,
                usage: Default::default(),
                inference: Default::default(),
            })])
            .unwrap();
        assert_eq!(tool_calls(&terminal), usize::from(successful));
        assert_eq!(receipts(&terminal), usize::from(successful));
        if successful {
            let call_start = terminal
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        HistoryEvent::ContentBlockStart {
                            content_block: HistoryBlock::ToolCall { .. },
                            ..
                        }
                    )
                })
                .unwrap();
            assert_eq!(
                receipts(&terminal[..call_start]),
                1,
                "native provenance must precede any schedulable tool call"
            );
            assert!(
                terminal[..call_start].iter().any(|event| matches!(
                    event,
                    HistoryEvent::ContentBlockStop { index: 0x8000_0000 }
                )),
                "the receipt must be complete before tool execution can cancel the turn"
            );
            assert_eq!(
                terminal
                    .iter()
                    .filter(|event| matches!(event, HistoryEvent::ContentBlockStop { index: 0 }))
                    .count(),
                1,
                "a buffered call completed by SDK before End still receives its host block stop"
            );
            assert_eq!(
                terminal
                    .iter()
                    .filter(|event| matches!(
                        event,
                        HistoryEvent::ContentBlockStop { index: 0x8000_0000 }
                    ))
                    .count(),
                1,
                "the delayed durable receipt is also closed exactly once"
            );
        }
        let duplicate_terminal = projector
            .events(vec![Ok(wire::StreamEvent::End {
                stop_reason: wire::StopReason::Other("requires_action".into()),
                usage: Default::default(),
                inference: Default::default(),
            })])
            .unwrap();
        assert_eq!(
            tool_calls(&duplicate_terminal),
            0,
            "terminal replay never duplicates a dispatch"
        );
        assert_eq!(receipts(&duplicate_terminal), 0);
    }
    let mut canceled = native_projector();
    assert_eq!(
        tool_calls(&canceled.events(vec![native_event()]).unwrap()),
        0
    );
    drop(canceled);
    let mut failed = native_projector();
    assert_eq!(tool_calls(&failed.events(vec![native_event()]).unwrap()), 0);
    let result = failed.events(vec![Err(wire::LlmError::InvalidRequest {
        message: "synthetic failed body".into(),
    })]);
    match result {
        Err(_) => {}
        Ok(events) => {
            assert_eq!(tool_calls(&events), 0);
            assert!(failed.take_error().is_some());
        }
    }
}

#[test]
fn non_action_complete_response_keeps_native_calls_opaque() {
    for stop in ["max_tokens", "refusal", "failed", "end_turn"] {
        let mut response = crate::HistoryResponse {
            id: "fixture-response".into(),
            model: MODEL.into(),
            content: vec![HistoryBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: native_call(),
            }],
            stop_reason: Some(stop.into()),
            stop_details: None,
            usage: Default::default(),
            cost: None,
            provider_metadata: Default::default(),
            resumable: None,
        };
        let original = response.content.clone();
        crate::history_projection::admit_local_tool_search(&mut response).unwrap();
        assert_eq!(response.content, original);
        assert_eq!(response.stop_reason.as_deref(), Some(stop));
    }
}
