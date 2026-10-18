use lingxi_llm_client::codecs::anthropic::AnthropicMessagesCodec;
use lingxi_llm_client::codecs::{CodecContext, RequestMode, WireCodec};
use lingxi_llm_client::protocol::{LlmError, ProviderProfile, StreamEvent};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

struct Vector {
    id: &'static str,
    raw_events: &'static [&'static str],
    provider_events: &'static [&'static str],
    sdk_events: &'static [&'static str],
    pre_error_text: &'static str,
    error_kind: &'static str,
    expected_error_source: &'static str,
}

const VECTORS: [Vector; 3] = [
    Vector {
        id: "timeout-error-lone-surrogate",
        raw_events: &["error"],
        provider_events: &[],
        sdk_events: &["error"],
        pre_error_text: "",
        error_kind: "timeout_error",
        expected_error_source: "provider_timeout",
    },
    Vector {
        id: "timeout-error-after-text-delta",
        raw_events: &[
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error",
        ],
        provider_events: &["message_start"],
        sdk_events: &["start", "text_delta", "error"],
        pre_error_text: "partial-before-error: ",
        error_kind: "timeout_error",
        expected_error_source: "provider_timeout",
    },
    Vector {
        id: "api-error-lone-surrogate-control",
        raw_events: &["error"],
        provider_events: &[],
        sdk_events: &["error"],
        pre_error_text: "",
        error_kind: "api_error",
        expected_error_source: "provider_internal",
    },
];

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/statusless-stream-error/native-baseline")
}

fn raw_events(raw: &[u8]) -> Vec<(String, String)> {
    let text = std::str::from_utf8(raw).expect("fixture SSE is UTF-8");
    let mut current_event = None;
    let mut events = Vec::new();
    for line in text.lines() {
        if let Some(event) = line.strip_prefix("event: ") {
            current_event = Some(event.to_owned());
        } else if let Some(data) = line.strip_prefix("data: ") {
            if let Some(event) = current_event.take() {
                events.push((event, data.to_owned()));
            }
        }
    }
    events
}

fn raw_error_data(raw: &[u8]) -> String {
    raw_events(raw)
        .into_iter()
        .find_map(|(event, data)| (event == "error").then_some(data))
        .expect("fixture has no error event data")
}

fn units_hex(units: &[u16]) -> String {
    let mut hex = String::with_capacity(units.len() * 4);
    for unit in units {
        use std::fmt::Write;
        write!(&mut hex, "{unit:04x}").expect("write into String");
    }
    hex
}

fn error_source(error: &LlmError) -> (&'static str, Option<u16>) {
    match error.classification_source() {
        LlmError::ProviderTimeout { status, .. } => ("provider_timeout", *status),
        LlmError::ProviderInternal { .. } => ("provider_internal", None),
        other => panic!("unexpected current SDK classification: {other:?}"),
    }
}

#[test]
fn current_sdk_public_stream_api_decodes_the_exact_three_raw_sse_vectors() {
    let profile: ProviderProfile = serde_json::from_value(json!({
        "provider_id": "anthropic",
        "profile_name": "anthropic",
        "base_url": "https://api.anthropic.com",
        "protocol": "anthropic_messages",
        "auth": "none",
        "models": [{
            "display_model": "fixture-model",
            "request_model": "fixture-model",
            "billing_model": "fixture-model"
        }],
        "extra": {}
    }))
    .expect("first-party Anthropic profile");
    let context = CodecContext::new(&profile, "fixture-model", RequestMode::Stream);
    let fixture_root = fixture_root();

    for vector in VECTORS {
        let fixture_path = fixture_root.join(vector.id).join("http-response.sse.raw");
        let raw = fs::read(&fixture_path).expect("checked-in Native raw SSE vector");
        let raw_event_payloads = raw_events(&raw);
        let raw_events = raw_event_payloads
            .iter()
            .map(|(event, _)| event.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            raw_events.iter().map(String::as_str).collect::<Vec<_>>(),
            vector.raw_events,
            "fixture raw event order: {}",
            vector.id
        );
        let expected_error_text = raw_error_data(&raw);

        let mut decoder = AnthropicMessagesCodec.stream_decoder(&context);
        decoder.set_response_status(200);
        decoder
            .set_response_headers(&[("content-type".to_owned(), "text/event-stream".to_owned())]);
        let batch = decoder.push_bytes(&raw);
        let mut provider_events = Vec::new();
        let mut sdk_events = Vec::new();
        let mut text = String::new();
        let mut failure = None;
        for item in batch {
            match item {
                Ok(StreamEvent::ProviderEvent { protocol, payload }) => {
                    assert_eq!(
                        protocol,
                        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
                        "raw provider event protocol: {}",
                        vector.id
                    );
                    let event = payload
                        .string_at("/type")
                        .expect("raw provider payload has its event type");
                    provider_events.push((event, payload.as_json_source().to_owned()));
                }
                Ok(StreamEvent::Start { .. }) => sdk_events.push("start"),
                Ok(StreamEvent::TextDelta { text: delta, .. }) => {
                    sdk_events.push("text_delta");
                    text.push_str(&delta);
                }
                Ok(StreamEvent::TextDeltaJsUtf16 {
                    text: delta,
                    utf16_code_units,
                    ..
                }) => {
                    sdk_events.push("text_delta");
                    text.push_str(&delta);
                    assert_eq!(
                        String::from_utf16_lossy(&utf16_code_units),
                        delta,
                        "SDK text delta UTF-16 projection: {}",
                        vector.id
                    );
                }
                Err(error) => {
                    sdk_events.push("error");
                    failure = Some(error);
                }
                Ok(other) => panic!(
                    "unexpected normalized current SDK event for {}: {other:?}",
                    vector.id
                ),
            }
        }
        let expected_provider_events = vector
            .provider_events
            .iter()
            .map(|expected_event| {
                let (_, payload) = raw_event_payloads
                    .iter()
                    .find(|(event, _)| event == expected_event)
                    .expect("expected provider event exists in raw fixture");
                ((*expected_event).to_owned(), payload.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            provider_events, expected_provider_events,
            "raw ProviderEvent sequence and source payloads: {}",
            vector.id
        );
        assert_eq!(
            sdk_events, vector.sdk_events,
            "SDK event order: {}",
            vector.id
        );
        assert_eq!(
            text, vector.pre_error_text,
            "text before error: {}",
            vector.id
        );
        let failure = failure.expect("statusless event:error becomes an SDK failure");
        let source_text = failure
            .provider_error_text()
            .expect("SDK retains provider error source")
            .clone();
        assert_eq!(
            source_text.display(),
            expected_error_text,
            "provider error text is the exact raw SSE data field: {}",
            vector.id
        );
        assert_eq!(
            source_text.utf16_code_units(),
            expected_error_text.encode_utf16().collect::<Vec<_>>(),
            "provider error source UTF-16 units: {}",
            vector.id
        );
        let expected_nested_source = match vector.error_kind {
            "timeout_error" => r#""message":"native timeout \ud800 tail""#,
            "api_error" => r#""message":"native api \ud800 tail""#,
            other => panic!("unexpected fixture error kind: {other}"),
        };
        assert!(
            expected_error_text.contains(expected_nested_source),
            "fixture error source retains its escaped lone-surrogate input"
        );
        let (class, status) = error_source(&failure);
        assert_eq!(
            class, vector.expected_error_source,
            "error kind: {}",
            vector.id
        );
        assert_eq!(status, None, "statusless event error: {}", vector.id);
        let output = json!({
            "fixture_id": vector.id,
            "execution_label": "current llm-client public StreamDecoder API; raw 2.1.293 SSE input",
            "raw_event_sequence": raw_events,
            "raw_provider_event_sequence": vector.provider_events,
            "sdk_event_sequence": sdk_events,
            "pre_error_text": text,
            "provider_error_text": source_text.display(),
            "provider_error_text_utf16_hex": units_hex(source_text.utf16_code_units()),
            "error_source": class,
            "status_is_none": status.is_none(),
        });
        println!("R379_CURRENT_SDK_VECTOR={output}");
    }
}
