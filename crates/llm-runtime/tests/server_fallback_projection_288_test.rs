//! Server fallback response projection across the SDK transport and ApiService.

use async_trait::async_trait;
use futures::{stream, StreamExt};
use lingxi_llm_client::{
    protocol::LlmError as SdkError, HttpRequest, HttpResponse, StreamResponse,
};
use llm_runtime::history::{ContentBlock, HistoryEvent};
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    stream_accumulator::accumulate_stream_salvaging, ApiService, ClientConfig, CostEstimator,
    LlmRequest, ModelRuntime, NonStreamingRequestClass, NonStreamingRetryOptions, PricingCatalog,
    PricingPolicy, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const MODEL: &str = "claude-opus-4-7";
const REQUEST_ID: &str = "req_server_fallback_288";

#[derive(Clone)]
enum Frame {
    Json(Value),
    Disconnect,
}

struct FixtureTransport {
    response: Value,
    stream_frames: Vec<Frame>,
    requests: Mutex<Vec<Value>>,
}

impl FixtureTransport {
    fn new(response: Value, stream_frames: Vec<Frame>) -> Arc<Self> {
        Arc::new(Self {
            response,
            stream_frames,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for FixtureTransport {
    async fn send(&self, request: HttpRequest) -> Result<StreamResponse, SdkError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        self.requests.lock().unwrap().push(body.clone());
        let headers = vec![("request-id".into(), REQUEST_ID.into())];
        if body["stream"] == true {
            let chunks = self
                .stream_frames
                .clone()
                .into_iter()
                .map(|frame| match frame {
                    Frame::Json(frame) => Ok(bytes::Bytes::from(format!("data: {frame}\n\n"))),
                    Frame::Disconnect => Err(SdkError::Transport {
                        message: "fixture stream disconnected".into(),
                    }),
                })
                .collect::<Vec<_>>();
            return Ok(StreamResponse {
                status: 200,
                headers,
                body: stream::iter(chunks).boxed(),
            });
        }

        Ok(HttpResponse {
            status: 200,
            headers,
            body: serde_json::to_vec(&self.response).unwrap().into(),
        }
        .into())
    }
}

fn service(transport: Arc<FixtureTransport>, custom: bool) -> ApiService {
    service_with_estimator(transport, custom, None)
}

fn priced_service(transport: Arc<FixtureTransport>, custom: bool) -> ApiService {
    service_with_estimator(
        transport,
        custom,
        Some(Arc::new(CostEstimator::new(
            PricingCatalog::empty(),
            PricingPolicy::MarkUnestimated,
        ))),
    )
}

fn service_with_estimator(
    transport: Arc<FixtureTransport>,
    custom: bool,
    estimator: Option<Arc<CostEstimator>>,
) -> ApiService {
    let provider = if custom {
        json!({"custom":{"name":"gateway"}})
    } else {
        json!("anthropic_first_party")
    };
    // This deliberately synthetic tariff belongs to this physical integration
    // fixture only; it makes the quote path deterministic without asserting a
    // production Claude price.
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":provider,
        "profile_name":"direct",
        "base_url":if custom {"https://gateway.invalid"} else {"https://api.anthropic.com"},
        "protocol":"anthropic_messages",
        "auth":"none",
        "credential":{"type":"none"},
        "pricing":{"billingMode":"perToken","overrides":[[MODEL,{
            "inputPerMtok":5.0,"outputPerMtok":25.0,
            "cacheReadPerMtok":0.5,"cacheWritePerMtok":6.25,
            "reasoningPerMtok":25.0
        }]]},
        "models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,
            "capabilities":{"streaming":true,"tools":true,"reasoning":true,"vision":false,
                "documents":false,"structured_output":false}}]
    }]}))
    .unwrap();
    assert_eq!(
        config.providers[0].pricing.billing_mode,
        Some(lingxi_core::host::ModelBillingMode::PerToken),
        "the quote fixture must configure billing mode at the host-supported provider level"
    );
    assert_eq!(
        config.providers[0].pricing.overrides[0].1.input_per_million, 5.0,
        "the synthetic integration tariff must survive host config deserialization"
    );
    ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(config).unwrap()),
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "fixture",
        None,
        None,
        estimator,
        Default::default(),
        Some(0),
        None,
    )
}

fn fallback_policy() -> lingxi_llm_client::providers::anthropic::fallback_request::RequestPolicy {
    use lingxi_llm_client::providers::anthropic::fallback_request::{
        LaneMode, RequestPolicy, ServerLane,
    };
    RequestPolicy {
        lane: Some(ServerLane {
            for_model: MODEL.into(),
            model: "host-target".into(),
            mode: LaneMode::Explicit,
        }),
        explicit_target_eligible: true,
        beta_transport_enabled: true,
        ..Default::default()
    }
}

fn request(armed: bool) -> LlmRequest {
    let mut request = LlmRequest::new(MODEL).with_user_text("fixture prompt");
    if armed {
        request.execution.server_fallback = Some(fallback_policy());
    }
    request
}

fn event(frame: Value) -> Frame {
    Frame::Json(frame)
}

fn message_start() -> Frame {
    message_start_model(MODEL)
}

fn message_start_model(model: &str) -> Frame {
    event(json!({"type":"message_start","message":{"id":"msg_fallback","model":model}}))
}

fn block_start(index: i64, content_block: Value) -> Frame {
    event(json!({"type":"content_block_start","index":index,"content_block":content_block}))
}

fn block_stop(index: i64) -> Frame {
    event(json!({"type":"content_block_stop","index":index}))
}

fn text_block(index: i64, text: &str, cited: bool) -> Vec<Frame> {
    let mut block = json!({"type":"text","text":text});
    if cited {
        block["citations"] = json!([{
            "type":"char_location","start_char_index":0,"end_char_index":text.len(),
            "document_index":0
        }]);
    }
    vec![block_start(index, block), block_stop(index)]
}

fn thinking_block(index: i64) -> Vec<Frame> {
    vec![
        block_start(index, json!({"type":"thinking","thinking":""})),
        event(json!({"type":"content_block_delta","index":index,
            "delta":{"type":"thinking_delta","thinking":"private thought"}})),
        event(json!({"type":"content_block_delta","index":index,
            "delta":{"type":"signature_delta","signature":"signature"}})),
        block_stop(index),
    ]
}

fn tool_block(index: i64) -> Vec<Frame> {
    vec![
        block_start(
            index,
            json!({"type":"tool_use","id":"tool_read","name":"Read","input":{}}),
        ),
        event(json!({"type":"content_block_delta","index":index,
            "delta":{"type":"input_json_delta","partial_json":"{}"}})),
        block_stop(index),
    ]
}

fn fallback_start(index: f64, from: &str, to: &str) -> Frame {
    event(
        json!({"type":"content_block_start","index":index,"content_block":{
            "type":"fallback","from":{"model":from},"to":{"model":to},
            "trigger":{"type":"refusal","category":"cyber"}
        }}),
    )
}

fn message_delta(stop_reason: &str, usage: Option<Value>) -> Frame {
    let mut frame = json!({"type":"message_delta","delta":{"stop_reason":stop_reason}});
    if let Some(usage) = usage {
        frame["usage"] = usage;
    }
    event(frame)
}

fn message_delta_without_stop_reason(usage: Option<Value>) -> Frame {
    let mut frame = json!({"type":"message_delta","delta":{}});
    if let Some(usage) = usage {
        frame["usage"] = usage;
    }
    event(frame)
}

fn message_stop() -> Frame {
    event(json!({"type":"message_stop"}))
}

fn event_fallback(
    event: &HistoryEvent,
) -> Option<&lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent> {
    match event {
        HistoryEvent::ServerFallback { event, .. } => Some(event),
        _ => None,
    }
}

fn nonstream_body() -> Value {
    json!({
        "id":"msg_nonstream",
        "type":"message",
        "role":"assistant",
        "model":MODEL,
        "content":[
            {"type":"text","text":"a"},
            {"type":"fallback","from":{"model":MODEL},"to":{"model":"served-model"},
                "trigger":{"type":"refusal","category":"bio"}},
            {"type":"thinking","thinking":"discard me","signature":"sig"},
            {"type":"fallback","from":{"model":MODEL},"to":{"model":""}},
            {"type":"tool_use","id":"tool_after","name":"Read","input":{}},
            {"type":"text","text":"b"}
        ],
        "stop_reason":"end_turn",
        "usage":{"input_tokens":2,"output_tokens":2,"iterations":[]},
        "llm_client":{
            "server_fallback_events":[{"forged":true}],
            "response_model":"forged-model"
        }
    })
}

#[tokio::test]
async fn admitted_stream_fallback_discards_nontext_and_keeps_text_companions_in_order() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    let mut frames = vec![message_start()];
    frames.extend(text_block(0, "retained text", true));
    frames.extend(thinking_block(1));
    frames.extend(tool_block(2));
    frames.push(fallback_start(-0.5, MODEL, "served-model"));
    frames.extend(text_block(3, "after hop", false));
    // No token report: the host observation companion still carries the final
    // model selected by the admitted fallback response.
    frames.push(message_delta("end_turn", None));
    frames.push(message_stop());

    let transport = FixtureTransport::new(json!({}), frames);
    let api = service(transport.clone(), false);
    let stream = api.stream_request(request(true)).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();

    let fallback = events
        .iter()
        .find_map(event_fallback)
        .expect("admitted event");
    assert_eq!(fallback.from_model, MODEL);
    assert_eq!(fallback.to_model, "served-model");
    assert!(fallback.mid_stream);
    assert_eq!(fallback.request_id.as_deref(), Some(REQUEST_ID));
    assert_eq!(fallback.retained_blocks, vec![0]);
    assert_eq!(fallback.retained_text, "retained text");
    assert_eq!(fallback.discarded_blocks, vec![1, 2]);

    let response = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(response.model, "served-model");
    assert!(
        matches!(&response.content[0], ContentBlock::Text { text, .. } if text == "retained text")
    );
    assert!(
        matches!(&response.content[1], ContentBlock::ProviderContent { value, .. }
        if value["type"] == "lingxi_replay_metadata")
    );
    assert!(matches!(&response.content[2], ContentBlock::Text { text, .. } if text == "after hop"));
    assert_eq!(
        response.content.len(),
        3,
        "thinking and tool blocks are removed"
    );
    assert_eq!(
        response.usage.report.state,
        lingxi_llm_client::protocol::UsageState::Missing
    );
    assert_eq!(
        response.provider_metadata["llm_client"]["response_model"],
        "served-model"
    );
    assert_eq!(response.server_fallback_events().len(), 1);

    let sent = transport.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["fallbacks"], json!([{"model":"host-target"}]));
}

#[tokio::test]
async fn terminal_and_interrupted_boundaries_project_once_without_replaying_the_request() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    let boundary_frames = vec![
        message_start(),
        fallback_start(-0.5, MODEL, "served-at-boundary"),
        message_delta(
            "end_turn",
            Some(
                json!({"output_tokens":1,"iterations":[{"type":"fallback_message","model":"usage-target"}]}),
            ),
        ),
        message_stop(),
    ];
    let transport = FixtureTransport::new(json!({}), boundary_frames);
    let api = service(transport.clone(), false);
    let stream = api.stream_request(request(true)).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    let fallback_index = events
        .iter()
        .position(|event| matches!(event, HistoryEvent::ServerFallback { .. }))
        .expect("terminal fallback event");
    let delta_index = events
        .iter()
        .position(|event| matches!(event, HistoryEvent::MessageDelta { .. }))
        .expect("terminal message delta");
    assert!(
        fallback_index < delta_index,
        "fallback waits for and precedes its boundary"
    );
    let fallback = event_fallback(&events[fallback_index]).unwrap();
    assert!(!fallback.mid_stream);
    assert_eq!(fallback.final_stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(fallback.request_id.as_deref(), Some(REQUEST_ID));
    let response = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(response.model, "served-at-boundary");
    assert_eq!(
        transport.requests().len(),
        1,
        "server controls never trigger a host retry"
    );

    let sticky_frames = vec![
        message_start(),
        message_delta(
            "end_turn",
            Some(json!({"input_tokens":1,"output_tokens":1,
                "iterations":[{"type":"fallback_message","model":""}]})),
        ),
        message_stop(),
    ];
    let sticky_transport = FixtureTransport::new(json!({}), sticky_frames);
    let sticky_api = service(sticky_transport.clone(), false);
    let stream = sticky_api.stream_request(request(true)).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    let fallback = events
        .iter()
        .find_map(event_fallback)
        .expect("usage-only sticky event");
    assert_eq!(fallback.reason, "sticky");
    assert_eq!(
        fallback.to_model, "",
        "an empty served model remains an observation"
    );
    assert_eq!(fallback.request_id.as_deref(), Some(REQUEST_ID));
    let response = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(response.model, "");

    let interrupted_frames = vec![
        message_start(),
        fallback_start(-0.5, MODEL, "served-before-error"),
        Frame::Disconnect,
    ];
    let interrupted_transport = FixtureTransport::new(json!({}), interrupted_frames);
    let interrupted_api = service(interrupted_transport.clone(), false);
    let mut stream = interrupted_api.stream_request(request(true)).await.unwrap();
    let mut observed = Vec::new();
    while let Some(item) = stream.next().await {
        observed.push(item);
    }
    let fallback_index = observed
        .iter()
        .position(|item| matches!(item, Ok(HistoryEvent::ServerFallback { .. })))
        .expect("pending hop is flushed before the transport error");
    let error_index = observed
        .iter()
        .position(Result::is_err)
        .expect("stream error");
    assert!(fallback_index < error_index);
    let fallback = event_fallback(observed[fallback_index].as_ref().unwrap()).unwrap();
    assert_eq!(fallback.to_model, "served-before-error");
    assert_eq!(fallback.request_id.as_deref(), Some(REQUEST_ID));
    assert_eq!(interrupted_transport.requests().len(), 1);
}

#[tokio::test]
async fn later_suppressed_hop_updates_response_model_without_a_second_controller_event() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    let mut frames = vec![message_start()];
    frames.extend(thinking_block(0));
    frames.extend(tool_block(1));
    frames.push(fallback_start(-0.5, MODEL, "first-served-model"));
    frames.push(fallback_start(
        -1.5,
        "first-served-model",
        "second-served-model",
    ));
    frames.push(message_delta("end_turn", None));
    frames.push(message_stop());

    let transport = FixtureTransport::new(json!({}), frames);
    let api = service(transport.clone(), false);
    let stream = api.stream_request(request(true)).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    let fallbacks = events.iter().filter_map(event_fallback).collect::<Vec<_>>();
    assert_eq!(
        fallbacks.len(),
        1,
        "the later pending hop is not a second controller event"
    );
    assert!(fallbacks[0].mid_stream);
    assert_eq!(fallbacks[0].to_model, "first-served-model");
    assert_eq!(fallbacks[0].discarded_blocks, vec![0, 1]);

    let response = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(response.model, "second-served-model");
    assert!(response.content.is_empty());
    assert_eq!(
        response.usage.report.state,
        lingxi_llm_client::protocol::UsageState::Missing
    );
    assert_eq!(
        response.provider_metadata["llm_client"]["response_model"],
        "second-served-model"
    );
    assert_eq!(transport.requests().len(), 1);

    // When completed text survives each hop, native behavior emits both
    // mid-stream projection events and keeps the final content only once.
    let mut text_frames = vec![message_start()];
    text_frames.extend(text_block(0, "kept", false));
    text_frames.push(fallback_start(-0.5, MODEL, "first-served-model"));
    text_frames.push(fallback_start(
        -1.5,
        "first-served-model",
        "second-served-model",
    ));
    text_frames.push(message_delta("end_turn", None));
    text_frames.push(message_stop());
    let text_transport = FixtureTransport::new(json!({}), text_frames);
    let text_api = service(text_transport.clone(), false);
    let stream = text_api.stream_request(request(true)).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    let fallbacks = events.iter().filter_map(event_fallback).collect::<Vec<_>>();
    assert_eq!(fallbacks.len(), 2);
    assert_eq!(fallbacks[0].retained_text, "kept");
    assert_eq!(fallbacks[1].from_model, "first-served-model");
    assert_eq!(fallbacks[1].to_model, "second-served-model");
    let response = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(response.model, "second-served-model");
    assert!(matches!(&response.content[..], [ContentBlock::Text { text, .. }] if text == "kept"));
    assert_eq!(text_transport.requests().len(), 1);
}

#[tokio::test]
async fn nonstream_projection_ignores_spoofed_metadata_and_isolated_lanes() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    let transport = FixtureTransport::new(nonstream_body(), vec![]);
    let api = service(transport.clone(), false);
    let response = api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(response.model, "served-model");
    assert_eq!(response.content.len(), 3);
    assert!(matches!(&response.content[0], ContentBlock::Text { text, .. } if text == "a"));
    assert!(matches!(&response.content[1], ContentBlock::ToolCall { name, .. } if name == "Read"));
    assert!(matches!(&response.content[2], ContentBlock::Text { text, .. } if text == "b"));
    let observed = response.server_fallback_events();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].event.to_model, "served-model");
    assert_eq!(observed[0].event.request_id.as_deref(), Some(REQUEST_ID));
    assert_eq!(observed[0].profile, "direct");
    assert_eq!(
        response.provider_metadata["llm_client"]["anthropic_fallback"]["malformedBlocks"],
        1
    );
    assert!(response.provider_metadata["llm_client"]["response_model"].is_null());
    assert_eq!(
        transport.requests().len(),
        1,
        "fallback controls are not a retry signal"
    );
    assert_eq!(
        transport.requests()[0]["fallbacks"],
        json!([{"model":"host-target"}])
    );

    let sticky_body = json!({
        "id":"msg_sticky",
        "type":"message",
        "role":"assistant",
        "model":MODEL,
        "content":[{"type":"text","text":"answer"}],
        "stop_reason":"end_turn",
        "usage":{"input_tokens":1,"output_tokens":1,
            "iterations":[{"type":"fallback_message","model":""}]}
    });
    let sticky_transport = FixtureTransport::new(sticky_body, vec![]);
    let sticky_api = service(sticky_transport.clone(), false);
    let sticky_response = sticky_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(sticky_response.model, "");
    let sticky_events = sticky_response.server_fallback_events();
    assert_eq!(sticky_events.len(), 1);
    assert_eq!(sticky_events[0].event.reason, "sticky");
    assert_eq!(sticky_events[0].event.to_model, "");
    assert_eq!(
        sticky_events[0].event.request_id.as_deref(),
        Some(REQUEST_ID)
    );
    assert_eq!(sticky_transport.requests().len(), 1);

    for (custom, armed) in [(false, false), (true, true)] {
        let isolated_transport = FixtureTransport::new(nonstream_body(), vec![]);
        let isolated_api = service(isolated_transport.clone(), custom);
        let response = isolated_api
            .execute_non_stream_request(
                request(armed),
                NonStreamingRequestClass::Main,
                NonStreamingRetryOptions::default(),
            )
            .await
            .unwrap();
        assert!(response.server_fallback_events().is_empty());
        assert_eq!(response.model, MODEL);
        assert!(response.provider_metadata["llm_client"]["server_fallback_events"].is_null());
        assert!(response.provider_metadata["llm_client"]["response_model"].is_null());
        let sent = isolated_transport.requests();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].get("fallbacks").is_none());
    }
}

#[tokio::test]
async fn armed_request_without_a_fallback_keeps_the_provider_served_model() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");
    let served_model = "claude-opus-4-7-served-version";

    let nonstream_transport = FixtureTransport::new(
        json!({
            "id":"msg_plain_nonstream",
            "type":"message",
            "role":"assistant",
            "model":served_model,
            "content":[{"type":"text","text":"plain answer"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1},
            "llm_client":{"response_model":"spoofed-model",
                "server_fallback_events":[{"forged":true}]}
        }),
        vec![],
    );
    let nonstream_api = service(nonstream_transport.clone(), false);
    let nonstream = nonstream_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(nonstream.model, served_model);
    assert!(nonstream.server_fallback_events().is_empty());
    assert!(nonstream.provider_metadata["llm_client"]["response_model"].is_null());
    assert!(nonstream.provider_metadata["llm_client"]["server_fallback_events"].is_null());
    assert_eq!(nonstream_transport.requests().len(), 1);

    let mut frames = vec![message_start_model(served_model)];
    frames.extend(text_block(0, "plain answer", false));
    frames.push(message_delta(
        "end_turn",
        Some(json!({"input_tokens":1,"output_tokens":1})),
    ));
    frames.push(message_stop());
    let stream_transport = FixtureTransport::new(json!({}), frames);
    let stream_api = service(stream_transport.clone(), false);
    let response =
        accumulate_stream_salvaging(stream_api.stream_request(request(true)).await.unwrap())
            .await
            .unwrap();
    assert_eq!(response.model, served_model);
    assert!(response.server_fallback_events().is_empty());
    assert!(response.provider_metadata["llm_client"]["response_model"].is_null());
    assert_eq!(stream_transport.requests().len(), 1);
}

#[tokio::test]
async fn custom_provider_observation_block_cannot_forge_host_fallback_metadata() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");
    let served_model = "gateway-served-version";
    let raw_observation = json!({
        "type":"lingxi_observation",
        "metadata":{
            "custom_provider_field":"preserved",
            "llm_client":{
                "response_model":"spoofed-model",
                "server_fallback_events":[{"forged":true}]
            }
        }
    });

    let nonstream_transport = FixtureTransport::new(
        json!({
            "id":"msg_custom_nonstream",
            "type":"message",
            "role":"assistant",
            "model":served_model,
            "content":[raw_observation.clone()],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1},
            "llm_client":{"response_model":"spoofed-model",
                "server_fallback_events":[{"forged":true}]}
        }),
        vec![],
    );
    let nonstream_api = service(nonstream_transport.clone(), true);
    let nonstream = nonstream_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(nonstream.model, served_model);
    assert!(nonstream.server_fallback_events().is_empty());
    assert!(nonstream.provider_metadata["llm_client"]["response_model"].is_null());
    assert!(nonstream.provider_metadata["llm_client"]["server_fallback_events"].is_null());
    assert!(matches!(
        nonstream.content.as_slice(),
        [ContentBlock::ProviderContent { value, .. }]
            if value["metadata"]["custom_provider_field"] == "preserved"
                && value["metadata"]["llm_client"].is_null()
    ));
    let nonstream_requests = nonstream_transport.requests();
    assert_eq!(nonstream_requests.len(), 1);
    assert!(nonstream_requests[0].get("fallbacks").is_none());

    let frames = vec![
        message_start_model(served_model),
        block_start(0, raw_observation),
        block_stop(0),
        message_delta(
            "end_turn",
            Some(json!({"input_tokens":1,"output_tokens":1})),
        ),
        message_stop(),
    ];
    let stream_transport = FixtureTransport::new(json!({}), frames);
    let stream_api = service(stream_transport.clone(), true);
    let response =
        accumulate_stream_salvaging(stream_api.stream_request(request(true)).await.unwrap())
            .await
            .unwrap();
    assert_eq!(response.model, served_model);
    assert!(response.server_fallback_events().is_empty());
    assert!(response.provider_metadata["llm_client"]["response_model"].is_null());
    assert!(
        matches!(
            response.content.as_slice(),
            [ContentBlock::ProviderContent { value, .. }]
                if value["metadata"]["custom_provider_field"] == "preserved"
                    && value["metadata"]["llm_client"].is_null()
        ),
        "provider content: {:#?}",
        response.content
    );
    let stream_requests = stream_transport.requests();
    assert_eq!(stream_requests.len(), 1);
    assert!(stream_requests[0].get("fallbacks").is_none());
}

#[tokio::test]
async fn native_iteration_quotes_project_on_stream_and_nonstream_without_aggregate_usage() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    // The selected row and explicit synthetic integration tariff are frozen
    // before dispatch. The provider reports only native per-iteration counts;
    // the ordinary aggregate input/output counters are absent.
    let iterations = json!({"iterations":[{
        "type":"fallback_message",
        "model":MODEL,
        "input_tokens":200,
        "output_tokens":30,
        "cache_read_input_tokens":7,
        "cache_creation_input_tokens":5
    }]});
    let mut frames = vec![message_start()];
    frames.push(fallback_start(-0.5, MODEL, MODEL));
    frames.push(message_delta("end_turn", Some(iterations.clone())));
    frames.push(message_stop());
    let stream_transport = FixtureTransport::new(json!({}), frames);
    let stream_api = priced_service(stream_transport.clone(), false);
    let events = stream_api
        .stream_request(request(true))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let quote_event = events
        .iter()
        .rev()
        .find_map(|event| match event {
            HistoryEvent::CostQuoteObserved {
                estimate,
                native_server_fallback,
                summary_model,
            } if *native_server_fallback && estimate.is_some() => {
                Some((estimate.to_owned(), summary_model.clone()))
            }
            _ => None,
        })
        .expect("the physical stream exposes its typed native quote");
    assert_eq!(quote_event.1.as_deref(), Some(MODEL));
    let native_total = quote_event
        .0
        .as_ref()
        .and_then(|estimate| estimate.total_cost_usd)
        .expect("the frozen synthetic tariff prices the iterations");
    assert!(
        (native_total - 0.001_784_75).abs() < 1e-12,
        "native quote uses the configured input/output/cache rates: {native_total}"
    );

    let streamed = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(
        streamed.usage.report.state,
        lingxi_llm_client::protocol::UsageState::Partial,
        "iteration usage is not promoted into aggregate token counters"
    );
    let streamed_quote = streamed
        .server_fallback_cost_quote()
        .expect("host-owned quote metadata survives the stream");
    assert_eq!(streamed_quote["completeness"], "complete");
    assert_eq!(
        streamed_quote["quote"]["components"][0]["pricingSource"], "override",
        "the native component uses the configured provider override"
    );
    assert_eq!(
        streamed.cost.as_ref().and_then(|cost| cost.total_cost_usd),
        quote_event
            .0
            .as_ref()
            .and_then(|estimate| estimate.total_cost_usd),
        "the accumulator keeps the exact SDK iteration quote"
    );
    assert!(
        streamed
            .usage
            .provider_metadata
            .get("stream")
            .and_then(llm_runtime::history::server_fallback_cost_quote)
            .is_some(),
        "the usage stream carries the quote marker used by host settlement"
    );
    assert_eq!(stream_transport.requests().len(), 1);

    let nonstream_transport = FixtureTransport::new(
        json!({
            "id":"msg_iteration_quote",
            "type":"message",
            "role":"assistant",
            "model":MODEL,
            "content":[
                {"type":"text","text":"answer"},
                {"type":"fallback","from":{"model":MODEL},"to":{"model":MODEL},
                    "trigger":{"type":"refusal","category":"cyber"}}
            ],
            "stop_reason":"end_turn",
            "usage":iterations
        }),
        vec![],
    );
    let nonstream_api = priced_service(nonstream_transport.clone(), false);
    let nonstream = nonstream_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        nonstream.usage.report.state,
        lingxi_llm_client::protocol::UsageState::Partial
    );
    let nonstream_quote = nonstream
        .server_fallback_cost_quote()
        .expect("nonstream quote marker");
    assert_eq!(nonstream_quote["completeness"], "complete");
    assert_eq!(
        nonstream_quote["quote"]["components"][0]["pricingSource"],
        "override"
    );
    let nonstream_total = nonstream
        .cost
        .as_ref()
        .and_then(|estimate| estimate.total_cost_usd)
        .expect("the nonstream path uses the same frozen synthetic tariff");
    assert!((nonstream_total - 0.001_784_75).abs() < 1e-12);
    assert!(nonstream.cost.as_ref().is_some_and(|cost| {
        cost.total_cost_usd.is_some() && cost.pricing_model.request_model == MODEL
    }));
    assert_eq!(nonstream_transport.requests().len(), 1);

    let zero_iterations = json!({"iterations":[{
        "type":"fallback_message",
        "model":MODEL,
        "input_tokens":0,
        "output_tokens":0,
        "cache_read_input_tokens":0,
        "cache_creation_input_tokens":0
    }]});
    let zero_transport = FixtureTransport::new(
        json!({
            "id":"msg_zero_iteration_quote",
            "type":"message",
            "role":"assistant",
            "model":MODEL,
            "content":[{"type":"text","text":"empty billed work"}],
            "stop_reason":"end_turn",
            "usage":zero_iterations
        }),
        vec![],
    );
    let zero_api = priced_service(zero_transport.clone(), false);
    let zero_response = zero_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        zero_response
            .server_fallback_cost_quote()
            .expect("zero usage is still a native quote")["completeness"],
        "complete"
    );
    assert_eq!(
        zero_response
            .cost
            .as_ref()
            .and_then(|cost| cost.total_cost_usd),
        Some(0.0),
        "a complete zero-dollar quote remains distinct from an unpriced quote"
    );
    assert_eq!(zero_transport.requests().len(), 1);
}

#[tokio::test]
async fn incomplete_native_quote_suppresses_aggregate_fallback_cost_and_plain_armed_call_does_not()
{
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    let unknown_iterations = json!({"iterations":[{
        "type":"fallback_message",
        "model":"unpriced-executed-model",
        "input_tokens":50,
        "output_tokens":10
    }]});
    let mut unknown_frames = vec![message_start()];
    unknown_frames.push(fallback_start(-0.5, MODEL, "unpriced-executed-model"));
    unknown_frames.push(message_delta("end_turn", Some(unknown_iterations.clone())));
    unknown_frames.push(message_stop());
    let unknown_stream_transport = FixtureTransport::new(json!({}), unknown_frames);
    let unknown_stream_api = priced_service(unknown_stream_transport.clone(), false);
    let unknown_events = unknown_stream_api
        .stream_request(request(true))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let quote_event = unknown_events.iter().find_map(|event| match event {
        HistoryEvent::CostQuoteObserved {
            estimate,
            native_server_fallback,
            summary_model,
        } => Some((estimate, native_server_fallback, summary_model)),
        _ => None,
    });
    assert!(
        quote_event.is_some_and(|(estimate, native, summary_model)| {
            estimate.is_none()
                && *native
                && summary_model.as_deref() == Some("unpriced-executed-model")
        })
    );
    let unknown_stream =
        accumulate_stream_salvaging(stream::iter(unknown_events.into_iter().map(Ok)).boxed())
            .await
            .unwrap();
    assert!(unknown_stream.cost.is_none());
    assert_eq!(
        unknown_stream
            .server_fallback_cost_quote()
            .expect("stream keeps the incomplete marker")["completeness"],
        "incomplete"
    );
    assert!(unknown_stream
        .usage
        .provider_metadata
        .get("stream")
        .and_then(llm_runtime::history::server_fallback_cost_quote)
        .is_some());
    assert_eq!(unknown_stream_transport.requests().len(), 1);

    let transport = FixtureTransport::new(
        json!({
            "id":"msg_unpriced_iteration_quote",
            "type":"message",
            "role":"assistant",
            "model":MODEL,
            "content":[{"type":"text","text":"answer"}],
            "stop_reason":"end_turn",
            "usage":unknown_iterations
        }),
        vec![],
    );
    let api = priced_service(transport.clone(), false);
    let response = api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    let quote = response
        .server_fallback_cost_quote()
        .expect("incomplete native branch remains distinguishable");
    assert_eq!(quote["completeness"], "incomplete");
    assert!(
        response.cost.is_none(),
        "unknown iteration tariff is never priced as the dispatched model"
    );
    assert_eq!(transport.requests().len(), 1);

    let served_version = "claude-opus-4-7-served-version";
    let plain_transport = FixtureTransport::new(
        json!({
            "id":"msg_plain_quote",
            "type":"message",
            "role":"assistant",
            "model":served_version,
            "content":[{"type":"text","text":"ordinary answer"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":5,"output_tokens":2}
        }),
        vec![],
    );
    let plain_api = priced_service(plain_transport.clone(), false);
    let plain = plain_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(plain.model, served_version);
    assert!(plain.server_fallback_cost_quote().is_none());
    assert!(plain.cost.as_ref().is_some_and(|cost| {
        cost.total_cost_usd.is_some() && cost.pricing_model.request_model == MODEL
    }));
    assert_eq!(plain_transport.requests().len(), 1);

    let mut plain_frames = vec![message_start_model(served_version)];
    plain_frames.extend(text_block(0, "ordinary answer", false));
    plain_frames.push(message_delta(
        "end_turn",
        Some(json!({
            "input_tokens":5,
            "output_tokens":2,
            "cache_read_input_tokens":0,
            "cache_creation_input_tokens":0
        })),
    ));
    plain_frames.push(message_stop());
    let plain_stream_transport = FixtureTransport::new(json!({}), plain_frames);
    let plain_stream_api = priced_service(plain_stream_transport.clone(), false);
    let plain_stream = accumulate_stream_salvaging(
        plain_stream_api
            .stream_request(request(true))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(plain_stream.model, served_version);
    assert!(plain_stream.server_fallback_cost_quote().is_none());
    assert!(plain_stream.cost.as_ref().is_some_and(|cost| {
        cost.total_cost_usd.is_some() && cost.pricing_model.request_model == MODEL
    }));
    assert_eq!(plain_stream_transport.requests().len(), 1);
}

#[tokio::test]
async fn stream_quote_uses_terminal_refusal_facts_and_retracts_explicit_empty_iterations() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    // Early iteration facts are provisional. The terminal refusal boundary
    // replaces them with the final iteration list, and cNe excludes only that
    // final fallback row from the priced quote.
    let interim_iterations = json!({"iterations":[{
        "type":"fallback_message",
        "model":MODEL,
        "input_tokens":3,
        "output_tokens":2
    }]});
    let terminal_iterations = json!({
        "input_tokens":400,
        "output_tokens":200,
        "cache_read_input_tokens":0,
        "cache_creation_input_tokens":0,
        "iterations":[
        {
            "type":"fallback_message",
            "model":MODEL,
            "input_tokens":40,
            "output_tokens":12
        },
        {
            "type":"fallback_message",
            "model":MODEL,
            "input_tokens":10,
            "output_tokens":4
        }
    ]});
    let refusal_transport = FixtureTransport::new(
        json!({}),
        vec![
            message_start(),
            fallback_start(-0.5, MODEL, MODEL),
            message_delta_without_stop_reason(Some(interim_iterations)),
            message_delta("refusal", Some(terminal_iterations)),
            message_stop(),
        ],
    );
    let refusal_api = priced_service(refusal_transport.clone(), false);
    let refusal_events = refusal_api
        .stream_request(request(true))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let quote_events = refusal_events
        .iter()
        .filter_map(|event| match event {
            HistoryEvent::CostQuoteObserved {
                estimate,
                native_server_fallback,
                summary_model,
            } => Some((
                estimate.clone(),
                *native_server_fallback,
                summary_model.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        quote_events.len() >= 2,
        "the stream first marks, then prices"
    );
    assert!(quote_events[0].0.is_none());
    assert!(quote_events[0].1);
    assert_eq!(quote_events[0].2.as_deref(), Some(MODEL));
    let (final_estimate, final_is_native, final_summary_model) =
        quote_events.last().expect("terminal quote event");
    assert!(*final_is_native);
    assert_eq!(final_summary_model.as_deref(), Some(MODEL));
    let final_estimate = final_estimate
        .as_ref()
        .cloned()
        .expect("terminal facts produce a priced quote");
    assert!(
        (final_estimate.total_cost_usd.unwrap_or_default() - 0.0005).abs() < 1e-12,
        "refusal pricing excludes only the terminal fallback iteration"
    );

    let refusal_response =
        accumulate_stream_salvaging(stream::iter(refusal_events.into_iter().map(Ok)).boxed())
            .await
            .unwrap();
    let native_quote = refusal_response
        .server_fallback_cost_quote()
        .expect("the final refusal quote remains attached to the physical response");
    assert_eq!(native_quote["quote"]["stopReason"], "refusal");
    assert_eq!(native_quote["quote"]["excludedTerminalFallbackIndex"], 1);
    assert_eq!(
        native_quote["quote"]["components"][1]["kind"],
        "excluded_refusal_terminal"
    );
    assert_eq!(refusal_response.cost.as_ref(), Some(&final_estimate));
    assert_eq!(refusal_transport.requests().len(), 1);

    // A later explicit empty iteration array revokes an earlier cNe candidate.
    // In that case the ordinary aggregate quote is authoritative and no native
    // envelope may remain for downstream cost dispatch.
    let interim_native = json!({"iterations":[{
        "type":"fallback_message",
        "model":MODEL,
        "input_tokens":20,
        "output_tokens":6
    }]});
    let final_empty_iterations = json!({
        "input_tokens":80,
        "output_tokens":15,
        "cache_read_input_tokens":0,
        "cache_creation_input_tokens":0,
        "iterations":[]
    });
    let empty_transport = FixtureTransport::new(
        json!({}),
        vec![
            message_start(),
            fallback_start(-0.5, MODEL, MODEL),
            message_delta_without_stop_reason(Some(interim_native)),
            message_delta("end_turn", Some(final_empty_iterations)),
            message_stop(),
        ],
    );
    let empty_api = priced_service(empty_transport.clone(), false);
    let empty_events = empty_api
        .stream_request(request(true))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(empty_events.iter().any(|event| matches!(
        event,
        HistoryEvent::CostQuoteObserved {
            estimate: None,
            native_server_fallback: true,
            summary_model: Some(model),
        } if model == MODEL
    )));
    let final_aggregate = empty_events
        .iter()
        .rev()
        .find_map(|event| match event {
            HistoryEvent::CostQuoteObserved {
                estimate,
                native_server_fallback: false,
                summary_model: None,
            } => estimate.as_ref().cloned(),
            _ => None,
        })
        .expect("explicit empty final iterations select aggregate pricing");
    let empty_response =
        accumulate_stream_salvaging(stream::iter(empty_events.into_iter().map(Ok)).boxed())
            .await
            .unwrap();
    assert_eq!(empty_response.cost.as_ref(), Some(&final_aggregate));
    assert!(
        empty_response.server_fallback_cost_quote().is_none(),
        "the terminal projection clears the provisional native envelope"
    );
    assert!(
        empty_response
            .usage
            .provider_metadata
            .get("stream")
            .and_then(llm_runtime::history::server_fallback_cost_quote)
            .is_none(),
        "the terminal usage companion clears the provisional native envelope"
    );
    assert!(empty_response.cost.as_ref().is_some_and(|cost| {
        cost.total_cost_usd.is_some() && cost.pricing_model.request_model == MODEL
    }));
    assert_eq!(empty_transport.requests().len(), 1);
}

#[tokio::test]
async fn terminal_eof_preserves_incomplete_quote_and_unpriced_summary_identity() {
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    std::env::remove_var("CLAUDE_CODE_SIMULATE_PROXY_USAGE");

    // This report has complete ordinary request-model counters, so EOF's
    // aggregate quote would be available. The native iteration targets an
    // unconfigured model, however, and must remain explicitly unpriced.
    let usage = json!({
        "input_tokens":100,
        "output_tokens":50,
        "cache_read_input_tokens":0,
        "cache_creation_input_tokens":0,
        "iterations":[{
            "type":"fallback_message",
            "model":"unpriced-eof-model",
            "input_tokens":20,
            "output_tokens":3
        }]
    });
    let frames = vec![
        message_start(),
        fallback_start(-0.5, MODEL, "unpriced-eof-model"),
        message_delta("end_turn", Some(usage)),
        message_stop(),
    ];
    let transport = FixtureTransport::new(json!({}), frames);
    let api = priced_service(transport.clone(), false);
    let events = api
        .stream_request(request(true))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        HistoryEvent::CostQuoteObserved {
            estimate: None,
            native_server_fallback: true,
            summary_model: Some(model),
        } if model == "unpriced-eof-model"
    )));

    // Collecting through MessageStop also drains the service's final EOF pass.
    let response = accumulate_stream_salvaging(stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap();
    assert_eq!(
        response.usage.report.state,
        lingxi_llm_client::protocol::UsageState::Complete
    );
    assert_eq!(response.cost, None);
    let quote = response
        .server_fallback_cost_quote()
        .expect("EOF keeps the native unpriced marker");
    assert_eq!(quote["completeness"], "incomplete");
    assert_eq!(quote["summaryModel"], "unpriced-eof-model");
    assert_eq!(transport.requests().len(), 1);

    // The same EOF boundary preserves a complete native quote rather than
    // replacing its per-iteration total with complete aggregate request usage.
    let complete_usage = json!({
        "input_tokens":100,
        "output_tokens":50,
        "cache_read_input_tokens":0,
        "cache_creation_input_tokens":0,
        "iterations":[{
            "type":"fallback_message",
            "model":MODEL,
            "input_tokens":5,
            "output_tokens":2
        }]
    });
    let complete_frames = vec![
        message_start(),
        fallback_start(-0.5, MODEL, MODEL),
        message_delta("end_turn", Some(complete_usage)),
        message_stop(),
    ];
    let complete_transport = FixtureTransport::new(json!({}), complete_frames);
    let complete_api = priced_service(complete_transport.clone(), false);
    let complete_events = complete_api
        .stream_request(request(true))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let native_estimate = complete_events
        .iter()
        .find_map(|event| match event {
            HistoryEvent::CostQuoteObserved {
                estimate: Some(estimate),
                native_server_fallback: true,
                ..
            } => Some(estimate.clone()),
            _ => None,
        })
        .expect("the terminal native quote remains complete");
    let complete_response =
        accumulate_stream_salvaging(stream::iter(complete_events.into_iter().map(Ok)).boxed())
            .await
            .unwrap();
    assert_eq!(complete_response.cost, Some(native_estimate));
    assert_eq!(
        complete_response
            .server_fallback_cost_quote()
            .expect("complete EOF marker")["completeness"],
        "complete"
    );
    assert_eq!(complete_transport.requests().len(), 1);

    // Without any pricing snapshot the host still reports the native Ucn
    // summary identity from the SDK's typed iterations and lane.
    let no_snapshot_transport = FixtureTransport::new(
        json!({
            "id":"msg_unpriced_without_snapshot",
            "type":"message",
            "role":"assistant",
            "model":MODEL,
            "content":[{"type":"text","text":"answer"}],
            "stop_reason":"end_turn",
            "usage":{"iterations":[{
                "type":"fallback_message",
                "model":MODEL,
                "input_tokens":1,
                "output_tokens":1
            }]}
        }),
        vec![],
    );
    let no_snapshot_api = service(no_snapshot_transport.clone(), false);
    let no_snapshot_response = no_snapshot_api
        .execute_non_stream_request(
            request(true),
            NonStreamingRequestClass::Main,
            NonStreamingRetryOptions::default(),
        )
        .await
        .unwrap();
    let no_snapshot_quote = no_snapshot_response
        .server_fallback_cost_quote()
        .expect("missing frozen pricing still has typed model identity");
    assert_eq!(no_snapshot_quote["completeness"], "incomplete");
    assert_eq!(no_snapshot_quote["summaryModel"], MODEL);
    assert!(no_snapshot_quote["quote"].is_null());
    assert_eq!(no_snapshot_transport.requests().len(), 1);
}
