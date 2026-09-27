use llm_runtime::{LlmRequest, ProviderRequest, ProviderResponse, Usage, WireCodec};
use serde_json::Value;

#[derive(Debug)]
struct DummyCodec;

impl WireCodec for DummyCodec {
    fn encode_request(
        &self,
        request: &LlmRequest,
    ) -> Result<ProviderRequest, llm_runtime::LlmError> {
        Ok(ProviderRequest::post_json(
            "https://example.test/v1/messages",
            serde_json::json!({"model": request.model}),
        ))
    }

    fn decode_response(
        &self,
        response: ProviderResponse,
    ) -> Result<llm_runtime::LlmResponse, llm_runtime::LlmError> {
        assert_eq!(response.status, 200);
        Ok(llm_runtime::LlmResponse {
            id: "id".to_string(),
            model: "model".to_string(),
            content: vec![],
            stop_reason: None,
            stop_details: None,
            usage: Usage::default(),
            cost: None,
            provider_metadata: Value::default(),
        })
    }

    fn stream_decoder(&self) -> Box<dyn llm_runtime::StreamDecoder> {
        Box::new(llm_runtime::NoopStreamDecoder)
    }

    fn clone_box(&self) -> Box<dyn WireCodec> {
        Box::new(DummyCodec)
    }
}

fn wire_codec_round_trip(codec: &dyn WireCodec) -> (ProviderRequest, llm_runtime::LlmResponse) {
    let request = codec
        .encode_request(&LlmRequest::new("model-a"))
        .expect("request");
    let response = codec
        .decode_response(ProviderResponse::json(200, serde_json::json!({"ok": true})))
        .expect("response");

    (request, response)
}

#[test]
fn codec_returns_post_json_provider_request() {
    let request = DummyCodec
        .encode_request(&LlmRequest::new("model-a"))
        .unwrap();

    assert_eq!(request.method, "POST");
    assert_eq!(request.url, "https://example.test/v1/messages");
    assert_eq!(request.body_json["model"], "model-a");
}

#[test]
fn codec_works_through_trait_object_and_json_response() {
    let codec: Box<dyn WireCodec> = Box::new(DummyCodec);
    let (request, response) = wire_codec_round_trip(codec.as_ref());

    assert_eq!(request.method, "POST");
    assert_eq!(response.id, "id");
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
