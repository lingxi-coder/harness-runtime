//! Suggestion fork uses native streaming, per-block candidates, and request-local metadata.
use lingxi_llm_client::protocol::JsonPayload;
use lingxi_llm_client::providers::anthropic::system_prompt::{NativeSystemPrefix, PromptText};
use llm_runtime::test_support::{FixtureTransport, FrameStream};
use llm_runtime::{
    BoxFuture, LlmError, ProviderRequest, ProviderResponse, RawStreamFrame, StreamingResponse,
};
use serde_json::{json, Value};
use sidequery::{ProviderSideQueryClient, QuerySource, SideQueryClient, SideQueryRequest};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
struct Frames(VecDeque<Value>);
impl FrameStream for Frames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let next = self
            .0
            .pop_front()
            .map(|v| RawStreamFrame::new(v.to_string().into_bytes()));
        Box::pin(async move { Ok(next) })
    }
}
#[derive(Default)]
struct Transport {
    seen: Mutex<Vec<String>>,
}
impl FixtureTransport for Transport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.seen
            .lock()
            .unwrap()
            .push(request.body_json.as_json_source().to_owned());
        Box::pin(async {
            Ok(ProviderResponse::json(
                200,
                json!({
                    "id":"ordinary_side_query",
                    "type":"message",
                    "role":"assistant",
                    "model":"claude-sonnet-4-6",
                    "content":[{"type":"text","text":"ordinary response"}],
                    "stop_reason":"end_turn",
                    "usage":{"input_tokens":1,"output_tokens":1}
                }),
            ))
        })
    }
    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.seen
            .lock()
            .unwrap()
            .push(request.body_json.as_json_source().to_owned());
        Box::pin(async {
            Ok(StreamingResponse {
                status: 200,
                headers: std::collections::BTreeMap::from([
                    ("request-id".into(), "req_suggestion_http".into()),
                    ("x-request-id".into(), "wrong_alias".into()),
                ]),
                frames: Box::new(Frames(VecDeque::from([
                    json!({"type":"message_start","message":{"id":"suggestion","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":0,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"run tests"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"must not concatenate"}}),
                    json!({"type":"content_block_stop","index":1}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
                    json!({"type":"message_stop"}),
                ]))),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(Transport);

fn identity_policy_request(query_source: QuerySource, system_prompt: &str) -> SideQueryRequest {
    SideQueryRequest {
        model_source: None,
        enable_prompt_caching: false,
        model_attempt: None,
        api_attempts_left: None,
        native_prompt_context: None,
        model: "claude-sonnet-4-6".into(),
        profile: Some("parent".into()),
        system_prompt: Some(system_prompt.into()),
        system_prompt_input: None,
        typed_attachment_ids: HashSet::new(),
        task_deliveries: HashMap::new(),
        messages: vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "identity policy fixture".into(),
        )],
        tools: Vec::new(),
        tool_choice: None,
        output_format: None,
        max_tokens: Some(128),
        max_retries: 0,
        temperature: None,
        thinking: None,
        effort: None,
        stop_sequences: Vec::new(),
        query_source,
        skip_system_prompt_prefix: false,
    }
}

fn system_texts(wire: &Value) -> Vec<&str> {
    wire["system"]
        .as_array()
        .expect("Anthropic system blocks are captured")
        .iter()
        .map(|block| {
            block["text"]
                .as_str()
                .expect("each captured system block is text")
        })
        .collect()
}

#[tokio::test]
async fn prompt_suggestion_main_transition_keeps_identity_but_ordinary_side_query_does_not() {
    const HOST_IDENTITY: &str = "HOST_ROLE_SENTINEL";

    let config: llm_runtime::ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"parent","protocol":"anthropic_messages","base_url":"https://api.anthropic.com","auth":"none","credential":{"type":"none"},
        "models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]
    }]}))
    .unwrap();
    let transport = Arc::new(Transport::default());
    let service = Arc::new(
        llm_runtime::ApiService::new_with_routing(
            Arc::new(llm_runtime::ModelRuntime::from_config(config).unwrap()),
            transport.clone(),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            Some(0),
        )
        .with_native_system_prefix(NativeSystemPrefix {
            compatibility_version: "2.1.295".into(),
            entrypoint: "sdk-cli".into(),
            host_identity: PromptText::from_string(HOST_IDENTITY),
            attribution_enabled: true,
            assume_first_party_base_url: false,
        }),
    );
    let client = ProviderSideQueryClient::from_service(service);

    let mut suggestion_request =
        identity_policy_request(QuerySource::PromptSuggestion, "suggestion parent system");
    suggestion_request.max_tokens = None;
    client
        .query_fork(
            suggestion_request,
            sidequery::ForkedSideQueryOptions::default(),
        )
        .await
        .unwrap();
    client
        .query(identity_policy_request(
            QuerySource::Classifier,
            "ordinary side-query system",
        ))
        .await
        .unwrap();

    let captured = transport.seen.lock().unwrap();
    assert_eq!(captured.len(), 2);
    let suggestion_wire: Value = serde_json::from_str(&captured[0]).unwrap();
    let suggestion_system = system_texts(&suggestion_wire);
    assert_eq!(
        suggestion_wire["max_tokens"].as_u64(),
        Some(
            llm_runtime::model::context_window::default_output_tokens_for_model(
                "claude-sonnet-4-6",
            )
        ),
        "ProviderSideQueryClient must preserve the unset override and let the SDK prepare the current model-directory default"
    );
    assert_eq!(suggestion_system.len(), 3);
    assert!(suggestion_system[0].starts_with("x-anthropic-billing-header:"));
    assert_eq!(suggestion_system[1], HOST_IDENTITY);
    assert_eq!(suggestion_system[2], "suggestion parent system");

    let ordinary_wire: Value = serde_json::from_str(&captured[1]).unwrap();
    let ordinary_system = system_texts(&ordinary_wire);
    assert_eq!(ordinary_system.len(), 2);
    assert!(ordinary_system[0].starts_with("x-anthropic-billing-header:"));
    assert_eq!(ordinary_system[1], "ordinary side-query system");
    assert!(!ordinary_system.contains(&HOST_IDENTITY));
}

#[tokio::test]
async fn session_prompt_suggestion_streams_with_parent_prefix_candidates_and_http_request_id() {
    let config:llm_runtime::ClientConfig=serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"parent","protocol":"anthropic_messages","base_url":"https://api.anthropic.com","auth":"none","credential":{"type":"none"},
        "models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]
    }]})).unwrap();
    let transport = Arc::new(Transport::default());
    let service = Arc::new(llm_runtime::ApiService::new_with_routing(
        Arc::new(llm_runtime::ModelRuntime::from_config(config).unwrap()),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        Some(0),
    ));
    let client = ProviderSideQueryClient::from_service(service.clone());
    for pin in [false, true] {
        service.set_thinking(if pin {
            llm_runtime::model::thinking::ThinkingConfig::Adaptive
        } else {
            llm_runtime::model::thinking::ThinkingConfig::Disabled
        });
        telemetry::feature_flags::test_set_flag("tengu_basalt_spur", pin);
        telemetry::feature_flags::test_set_flag("tengu_basalt_scarp", pin);
        let parent_id = lingxi_core::types::MessageId::new();
        let result = client
            .query_fork(
                SideQueryRequest {
                    model_source: None,
                    enable_prompt_caching: true,
                    model_attempt: None,
                    api_attempts_left: None,
                    native_prompt_context: None,
                    model: "claude-sonnet-4-6".into(),
                    profile: Some("parent".into()),
                    system_prompt: Some("parent system".into()),
                    system_prompt_input: None,
                    typed_attachment_ids: HashSet::new(),
                    task_deliveries: HashMap::new(),
                    messages: vec![
                        lingxi_core::types::ConversationMessage::user(
                            lingxi_core::types::MessageId::new(),
                            "parent user".into(),
                        ),
                        lingxi_core::types::ConversationMessage::Assistant {
                            id: parent_id,
                            content: vec![lingxi_core::types::ContentBlock::Text {
                                text: "parent reply".into(),
                                citations: None,
                            }],
                            stop_reason: Some("end_turn".into()),
                            per_turn_effort: None,
                            resumable: None,
                        },
                        lingxi_core::types::ConversationMessage::user(
                            lingxi_core::types::MessageId::new(),
                            "predict my next prompt".into(),
                        ),
                    ],
                    tools: vec![llm_runtime::ToolDeclaration {
                        input_schema_projection: None,
                        name: "Read".into(),
                        description: JsonPayload::string("parent tool"),
                        input_schema: JsonPayload::parse(r#"{"type":"object","properties":{}}"#)
                            .unwrap(),
                        tool_type: None,
                        extra: Default::default(),
                        strict: false,
                        defer_loading: false,
                    }],
                    tool_choice: None,
                    output_format: None,
                    max_tokens: Some(128),
                    max_retries: 0,
                    temperature: None,
                    thinking: None,
                    effort: None,
                    stop_sequences: vec![],
                    query_source: QuerySource::PromptSuggestion,
                    skip_system_prompt_prefix: true,
                },
                sidequery::ForkedSideQueryOptions {
                    fork_point_message_id: Some(parent_id),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result
                .response
                .text
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some("run tests".encode_utf16().collect())
        );
        let candidates = result
            .candidates
            .iter()
            .map(|candidate| candidate.string_at("").expect("candidate string"))
            .collect::<Vec<_>>();
        assert_eq!(candidates, ["run tests", "must not concatenate"]);
        assert_eq!(result.request_id.as_deref(), Some("req_suggestion_http"));
        let requests = transport.seen.lock().unwrap();
        assert_eq!(requests.len(), 1 + usize::from(pin));
        let wire_source = requests.last().unwrap();
        let wire: Value = serde_json::from_str(wire_source)
            .expect("captured provider request source is valid fixture JSON");
        assert_eq!(wire["stream"], true);
        if pin {
            assert_eq!(
                wire["thinking"]["type"], "adaptive",
                "fork must inherit live thinking changes"
            );
        } else {
            assert_eq!(
                wire["thinking"],
                json!({"type":"disabled"}),
                "fork must inherit the explicitly disabled session mode"
            );
        }
        assert_eq!(wire["model"], "claude-sonnet-4-6");
        assert_eq!(wire["tools"][0]["name"], "Read");
        assert_eq!(wire["max_tokens"], 128);
        assert!(wire.to_string().contains("parent system"));
        assert_eq!(
            wire["messages"][1]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(
            wire["messages"][2]["content"][0]
                .get("cache_control")
                .is_none(),
            "the suggestion prompt must not create a cache write"
        );
        assert_eq!(
            wire["messages"][0]["content"][0]
                .get("cache_control")
                .is_some(),
            pin,
            "basalt_scarp steps the parent pin back while preserving the skip-write tail marker"
        );
    }
    telemetry::feature_flags::test_clear_flag("tengu_basalt_spur");
    telemetry::feature_flags::test_clear_flag("tengu_basalt_scarp");
}
