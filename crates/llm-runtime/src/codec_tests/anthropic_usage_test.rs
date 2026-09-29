use crate::upstream::codec_fixtures::{AnthropicMessagesCodec, FixtureCodec};
// Anthropic usage normalization and interrupted-stream cache accounting.
use llm_runtime::{normalize_anthropic_usage, ModelAttemptUsageCompleteness, RawStreamFrame};

#[test]
fn normalizes_anthropic_usage_into_independent_billing_buckets() {
    let value = serde_json::json!({
        "input_tokens": 100,
        "output_tokens": 20,
        "cache_creation_input_tokens": 30,
        "cache_read_input_tokens": 40,
        "server_tool_use": {
            "web_search_requests": 2
        }
    });

    let usage = normalize_anthropic_usage(&value);

    assert_eq!(usage.billable_tokens.input, 100);
    assert_eq!(usage.billable_tokens.output, 20);
    assert_eq!(usage.billable_tokens.cache_write, 30);
    assert_eq!(usage.billable_tokens.cache_read, 40);
    assert_eq!(usage.billable_tokens.reasoning_output, 0);
    assert_eq!(
        usage
            .server_tool_use
            .expect("server tool usage")
            .web_search_requests,
        2
    );
}

#[test]
fn interrupted_stream_retains_observed_one_hour_cache_tokens_as_partial() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();
    decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::to_vec(&serde_json::json!({
                "type":"message_start",
                "message":{
                    "id":"message-1", "model":"claude-sonnet-4-6", "content":[],
                    "usage":{
                        "input_tokens":10, "output_tokens":0,
                        "cache_creation_input_tokens":20,
                        "cache_creation":{"ephemeral_1h_input_tokens":15}
                    }
                }
            }))
            .unwrap(),
        ))
        .unwrap();
    assert!(
        decoder.finish().is_err(),
        "missing terminal event is still an interruption"
    );
    let (usage, completeness) = decoder.observed_usage().unwrap();
    assert_eq!(completeness, ModelAttemptUsageCompleteness::Partial);
    assert_eq!(usage.billable_tokens.cache_write, 20);
    assert_eq!(
        usage.provider_metadata["cache_creation"]["ephemeral_1h_input_tokens"],
        15
    );
    assert_eq!(usage.provider_metadata["upstreamUsageState"], "partial");
    assert!(usage.provider_metadata.get("input_tokens").is_none());
    assert!(usage.provider_metadata.get("output_tokens").is_none());
}
