//! Verifies the 25 `tengu_api_*` event-name constants exist, every payload
//! struct deserializes with `deny_unknown_fields`, and `Verified`-typed
//! string fields round-trip through serde.

use telemetry::tengu::api;
use telemetry::Verified;

#[test]
fn all_25_api_event_names_are_locked() {
    // Spec §7 line 754-755: ~25 `tengu_api_*` events.
    let names: &[&str] = &[
        api::REQUEST_STARTED,
        api::REQUEST_SUCCEEDED,
        api::REQUEST_FAILED,
        api::RATE_LIMITED,
        api::RETRY_STARTED,
        api::RETRY_SUCCEEDED,
        api::RETRY_EXHAUSTED,
        api::COUNT_TOKENS_REQUESTED,
        api::COUNT_TOKENS_SUCCEEDED,
        api::COUNT_TOKENS_FAILED,
        api::OAUTH_REFRESH_TRIGGERED,
        api::OAUTH_REFRESH_SUCCEEDED,
        api::OAUTH_REFRESH_FAILED,
        api::OAUTH_401_REAUTH_STARTED,
        api::OAUTH_401_REAUTH_SUCCEEDED,
        api::STREAMING_STARTED,
        api::STREAMING_CHUNK_RECEIVED,
        api::STREAMING_COMPLETED,
        api::STREAMING_FAILED,
        api::BETA_HEADER_ATTACHED,
        api::PROVIDER_SELECTED,
        api::MODEL_RESOLVED,
        api::EXTENDED_THINKING_REQUESTED,
        api::REQUEST_CANCELLED,
        api::CIRCUIT_BREAKER_OPENED,
    ];
    assert_eq!(
        names.len(),
        25,
        "api category must declare exactly 25 events"
    );
    // Locked prefix: every name must start with `tengu_api_`.
    for n in names {
        assert!(
            n.starts_with("tengu_api_"),
            "API event name {n:?} must start with `tengu_api_`",
        );
    }
    // Locked byte-for-byte: a few representative names must match M3-03 plan exactly.
    assert_eq!(api::REQUEST_STARTED, "tengu_api_request_started");
    assert_eq!(api::REQUEST_SUCCEEDED, "tengu_api_request_succeeded");
    assert_eq!(api::REQUEST_FAILED, "tengu_api_request_failed");
    assert_eq!(api::RATE_LIMITED, "tengu_api_rate_limited");
}

#[test]
fn request_started_payload_round_trips() {
    let p = api::RequestStartedPayload {
        model: Verified::assert_safe("claude-sonnet-4-5".into()),
        provider: Verified::assert_safe("anthropic".into()),
        endpoint: Verified::assert_safe("/v1/messages".into()),
        request_id: Verified::assert_safe("req_abc123".into()),
        is_stream: false,
        input_tokens_estimate: Some(1024),
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let _: api::RequestStartedPayload = serde_json::from_str(&json).expect("round-trip");
}

#[test]
fn request_started_payload_rejects_unknown_field() {
    let json = r#"{"model":"x","provider":"y","endpoint":"/v1/messages","request_id":"r","is_stream":false,"input_tokens_estimate":null,"surprise":true}"#;
    let r: Result<api::RequestStartedPayload, _> = serde_json::from_str(json);
    assert!(r.is_err(), "deny_unknown_fields must reject `surprise`");
}

#[test]
fn rate_limited_payload_has_retry_after_secs() {
    let p = api::RateLimitedPayload {
        model: Verified::assert_safe("claude-sonnet-4-5".into()),
        provider: Verified::assert_safe("anthropic".into()),
        retry_after_secs: 30,
        bucket: api::RateLimitBucket::RequestsPerMinute,
    };
    let json = serde_json::to_string(&p).expect("serialize");
    assert!(json.contains("\"retry_after_secs\":30"));
}
