//! Regression tests for codec trait test.

use lingxi_llm_client::{self as sdk, protocol as wire, WireCodec};
use llm_runtime::{ProviderRequest, ProviderResponse};
use serde_json::json;

fn profile() -> wire::ProviderProfile {
    serde_json::from_value(json!({
        "provider_id":"openai", "profile_name":"openai",
        "base_url":"https://example.test/v1", "protocol":wire::ProtocolFamily::OpenAiChat,
        "auth":"none", "models":[]
    }))
    .unwrap()
}

#[test]
fn sdk_codec_returns_post_json_provider_request() {
    let profile = profile();
    let context = sdk::CodecContext::new(&profile, "model-a", sdk::RequestMode::Complete);
    let input =
        serde_json::from_value::<wire::ChatRequest>(json!({"model":"model-a", "messages":[]}))
            .unwrap();
    let request = sdk::OpenAiChatCodec
        .encode_request(sdk::EncodeRequest::new(&input), &context)
        .unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, "https://example.test/v1/chat/completions");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap()["model"],
        "model-a"
    );
}

#[test]
fn sdk_codec_works_through_trait_object_and_json_response() {
    let profile = profile();
    let context = sdk::CodecContext::new(&profile, "model-a", sdk::RequestMode::Complete);
    let codec: Box<dyn WireCodec> = Box::new(sdk::OpenAiChatCodec);
    let response = sdk::HttpResponse {
        status: 200,
        headers: Default::default(),
        body: serde_json::to_vec(&json!({"id":"id", "model":"model-a", "choices":[{"message":{"role":"assistant", "content":"hello"}, "finish_reason":"stop"}]})).unwrap().into(),
    };
    let decoded = codec.decode_response(&response, &context).unwrap();
    assert_eq!(decoded.model, "model-a");
    assert!(decoded.response_id.is_some());
}

#[test]
fn provider_envelopes_round_trip_through_serde_with_headers_and_request_id() {
    let mut request = ProviderRequest::post_json(
        "https://example.test/v1/messages",
        serde_json::json!({"model": "model-a"}),
    );
    request
        .headers
        .insert("x-request-id".to_string(), "abc123".to_string());

    let mut response = ProviderResponse::json(201, serde_json::json!({"ok": true}));
    response
        .headers
        .insert("content-type".to_string(), "application/json".to_string());
    response.request_id = Some("req-1".to_string());

    let request_value = serde_json::to_value(&request).expect("serialize request");
    let response_value = serde_json::to_value(&response).expect("serialize response");

    let request_round_trip: ProviderRequest =
        serde_json::from_value(request_value).expect("request round trip");
    let response_round_trip: ProviderResponse =
        serde_json::from_value(response_value).expect("response round trip");

    assert_eq!(request_round_trip, request);
    assert_eq!(response_round_trip, response);
}

#[test]
fn normalized_headers_keep_a_single_value_per_name() {
    let mut request =
        ProviderRequest::post_json("https://example.test/v1/messages", serde_json::json!({}));
    request
        .headers
        .insert("x-dup".to_string(), "one".to_string());
    request
        .headers
        .insert("x-dup".to_string(), "two".to_string());

    assert_eq!(request.headers.len(), 1);
    assert_eq!(
        request.headers.get("x-dup").map(String::as_str),
        Some("two")
    );
}
