//! Suggestion fork uses native streaming, per-block candidates, and request-local metadata.
use lingxi_llm_client::protocol::JsonPayload;
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
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async { panic!("suggestions must stream through the parent service") })
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
                    model_attempt: None,
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
                    max_tokens: 128,
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
        assert_eq!(result.candidates, ["run tests", "must not concatenate"]);
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
            assert!(wire.get("thinking").is_none());
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
