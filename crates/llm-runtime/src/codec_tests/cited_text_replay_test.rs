use crate::upstream::codec_fixtures::HistoryFixture;
use crate::upstream::codec_fixtures::{AnthropicMessagesCodec, FixtureCodec, OpenAiChatCodec};
// Cited provider text stays visible and is replayed exactly once.
use futures::StreamExt;
use llm_runtime::stream_accumulator::accumulate_stream_salvaging;
use llm_runtime::{ContentBlock, HistoryResponse, Message, ProviderResponse, RawStreamFrame};
use serde_json::{json, Value};

const ANSWER: &str = "The cited answer.";

fn citation() -> Value {
    json!({
        "type": "web_search_result_location", "url": "https://example.com",
        "title": "Source", "encrypted_index": "opaque", "cited_text": "answer"
    })
}

fn codec() -> AnthropicMessagesCodec {
    AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01")
}

async fn streamed_response(plain_first: bool, initial_text: bool) -> HistoryResponse {
    let codec = codec();
    let mut decoder = codec.stream_decoder();
    let mut frames = vec![json!({"type":"message_start","message":{
        "id":"msg-1","model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":0}
    }})];
    if plain_first {
        frames.extend([
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":ANSWER}}),
            json!({"type":"content_block_stop","index":0}),
        ]);
    }
    let index = usize::from(plain_first);
    frames.push(
        json!({"type":"content_block_start","index":index,"content_block":{
            "type":"text","text":if initial_text { ANSWER } else { "" }
        }}),
    );
    if !initial_text {
        frames.push(json!({"type":"content_block_delta","index":index,"delta":{
            "type":"text_delta","text":ANSWER
        }}));
    }
    frames.extend([
        json!({"type":"content_block_delta","index":index,"delta":{"type":"citations_delta","citation":citation()}}),
        json!({"type":"content_block_stop","index":index}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]);
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());
    accumulate_stream_salvaging(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap()
}

fn visible_text(response: &HistoryResponse) -> Vec<&str> {
    response
        .content
        .iter()
        .filter_map(ContentBlock::visible_text)
        .collect()
}

fn replay_request(response: HistoryResponse) -> HistoryFixture {
    let mut request = HistoryFixture::new("claude-sonnet-4-6");
    request.messages.push(Message { api_output_config: None,
        role: "assistant".into(),
        content: response.content,
    });
    request
}

async fn streamed_initial_citations(citations: Option<Value>) -> HistoryResponse {
    let mut decoder = codec().stream_decoder();
    let mut content_block = json!({"type":"text","text":"answer"});
    if let Some(citations) = citations {
        content_block["citations"] = citations;
    }
    let frames = [
        json!({"type":"message_start","message":{
            "id":"msg-initial-citations","model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":0}
        }}),
        json!({"type":"content_block_start","index":0,"content_block":content_block}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" tail"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());
    accumulate_stream_salvaging(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap()
}

async fn streamed_unknown_text_response() -> HistoryResponse {
    let mut decoder = codec().stream_decoder();
    let frames = [
        json!({"type":"message_start","message":{
            "id":"msg-opaque-text","model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":0}
        }}),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"text","text":"opaque ","provider_metadata":{"keep":"exactly"}
        }}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":citation()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());
    accumulate_stream_salvaging(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap()
}

async fn streamed_initial_and_late_citations() -> HistoryResponse {
    let initial = json!({"type":"char_location","start_char_index":0,"end_char_index":5});
    let late = json!({"type":"page_location","start_page_number":2,"end_page_number":3});
    let mut decoder = codec().stream_decoder();
    let frames = [
        json!({"type":"message_start","message":{
            "id":"msg-initial-and-late","model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":0}
        }}),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"text","text":"answer","citations":[initial]
        }}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":late}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());
    accumulate_stream_salvaging(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        .await
        .unwrap()
}

#[tokio::test]
async fn streamed_cited_text_stays_visible_and_replays_once_with_complete_citations() {
    for initial_text in [false, true] {
        let response = streamed_response(false, initial_text).await;
        assert_eq!(visible_text(&response), vec![ANSWER]);
        assert!(matches!(
            response.content.as_slice(),
            [ContentBlock::Text { citations: Some(Some(value)), .. }]
                if value == &json!([citation()])
        ));
        let request = replay_request(response);
        let encoded = codec().encode_request(&request).unwrap();
        let content = encoded.body_json["messages"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(content.len(), 1, "{}", encoded.body_json);
        assert_eq!(
            content[0],
            json!({"type":"text","text":ANSWER,"citations":[citation()]})
        );

        let foreign = OpenAiChatCodec::new("https://api.openai.com")
            .encode_request(&request)
            .unwrap();
        let body = foreign.body_json.to_string();
        assert_eq!(body.matches(ANSWER).count(), 1);
        assert!(!body.contains("encrypted_index"));
        assert!(!body.contains("lingxi_replay_metadata"));
    }
}

#[tokio::test]
async fn stream_text_keeps_absent_null_empty_and_nonempty_citation_presence() {
    let cases = [
        (None, None),
        (Some(json!(null)), Some(None)),
        (Some(json!([])), Some(Some(json!([])))),
        (Some(json!([citation()])), Some(Some(json!([citation()])))),
    ];
    for (wire_citations, expected) in cases {
        let response = streamed_initial_citations(wire_citations).await;
        assert!(matches!(
            response.content.as_slice(),
            [ContentBlock::Text { text, citations, .. }]
                if text == "answer tail" && citations == &expected
        ));
    }

    let combined = streamed_initial_and_late_citations().await;
    assert!(matches!(
        combined.content.as_slice(),
        [ContentBlock::Text { citations: Some(Some(value)), .. }]
            if value == &json!([
                {"type":"char_location","start_char_index":0,"end_char_index":5},
                {"type":"page_location","start_page_number":2,"end_page_number":3}
            ])
    ));
}

#[tokio::test]
async fn unknown_text_metadata_remains_one_raw_native_content_block() {
    let response = streamed_unknown_text_response().await;
    assert_eq!(visible_text(&response), vec!["opaque answer"]);
    assert_eq!(
        response.content.len(),
        1,
        "opaque text stays one source block"
    );
    assert!(matches!(
        response.content.as_slice(),
        [ContentBlock::ProviderContent { protocol, value }]
            if protocol == "anthropic_messages"
                && value["text"] == "opaque answer"
                && value["provider_metadata"] == json!({"keep":"exactly"})
                && value["citations"] == json!([citation()])
    ));

    let native = codec()
        .encode_request(&replay_request(response.clone()))
        .unwrap();
    assert_eq!(
        native.body_json["messages"][0]["content"],
        json!([{
            "type":"text",
            "text":"opaque answer",
            "provider_metadata":{"keep":"exactly"},
            "citations":[citation()]
        }])
    );
    let foreign = OpenAiChatCodec::new("https://api.openai.com")
        .encode_request(&replay_request(response))
        .unwrap();
    assert_eq!(
        foreign.body_json["messages"][0]["content"],
        json!("opaque answer")
    );
}

#[tokio::test]
async fn citations_stay_on_the_correct_block_when_plain_text_is_identical() {
    let response = streamed_response(true, false).await;
    assert_eq!(visible_text(&response), vec![ANSWER, ANSWER]);
    let encoded = codec().encode_request(&replay_request(response)).unwrap();
    let content = encoded.body_json["messages"][0]["content"]
        .as_array()
        .unwrap();
    assert_eq!(content.len(), 2, "{}", encoded.body_json);
    assert_eq!(content[0], json!({"type":"text","text":ANSWER}));
    assert_eq!(
        content[1],
        json!({"type":"text","text":ANSWER,"citations":[citation()]})
    );
}

#[test]
fn nonstream_cited_text_uses_the_same_visible_text_and_replay_contract() {
    let response = codec()
        .decode_response(ProviderResponse::json(
            200,
            json!({
                "id":"msg-1","type":"message","role":"assistant","model":"claude-sonnet-4-6",
                "content":[{"type":"text","text":ANSWER,"citations":[citation()]}],
                "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":5}
            }),
        ))
        .unwrap();
    assert_eq!(visible_text(&response), vec![ANSWER]);
    assert!(matches!(
        response.content.as_slice(),
        [ContentBlock::Text { citations: Some(Some(value)), .. }]
            if value == &json!([citation()])
    ));
    let encoded = codec().encode_request(&replay_request(response)).unwrap();
    assert_eq!(
        encoded.body_json["messages"][0]["content"],
        json!([{"type":"text","text":ANSWER,"citations":[citation()]}])
    );
}

#[test]
fn nonstream_text_presence_is_typed_and_unknown_fields_stay_native() {
    let decoded = codec()
        .decode_response(ProviderResponse::json(
            200,
            json!({
                "id":"msg-text-presence","type":"message","role":"assistant","model":"claude-sonnet-4-6",
                "content":[
                    {"type":"text","text":"absent"},
                    {"type":"text","text":"null","citations":null},
                    {"type":"text","text":"empty","citations":[]},
                    {"type":"text","text":"cited","citations":[citation()]},
                    {"type":"text","text":"opaque","citations":[citation()],"provider_metadata":{"keep":true}}
                ],
                "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":5}
            }),
        ))
        .unwrap();
    assert_eq!(decoded.content.len(), 5);
    assert!(matches!(
        &decoded.content[..4],
        [
            ContentBlock::Text { text: absent, citations: None, .. },
            ContentBlock::Text { text: null, citations: Some(None), .. },
            ContentBlock::Text { text: empty, citations: Some(Some(empty_values)), .. },
            ContentBlock::Text { text: cited, citations: Some(Some(cited_values)), .. },
        ] if absent == "absent"
            && null == "null"
            && empty == "empty"
            && empty_values == &json!([])
            && cited == "cited"
            && cited_values == &json!([citation()])
    ));
    assert!(matches!(
        &decoded.content[4],
        ContentBlock::ProviderContent { protocol, value }
            if protocol == "anthropic_messages"
                && value["text"] == "opaque"
                && value["provider_metadata"] == json!({"keep":true})
    ));
}
