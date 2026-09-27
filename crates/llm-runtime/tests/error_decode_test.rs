use std::time::Duration;

use llm_runtime::providers::GeminiCodec;
use llm_runtime::{AnthropicMessagesCodec, LlmError, OpenAiChatCodec, ProviderResponse, WireCodec};

fn anthropic_codec() -> AnthropicMessagesCodec {
    AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01")
}

fn openai_codec() -> OpenAiChatCodec {
    OpenAiChatCodec::new("https://api.openai.com/v1")
}

fn gemini_codec() -> GeminiCodec {
    GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta")
}

fn anthropic_error(status: u16, error_type: &str, message: &str) -> ProviderResponse {
    ProviderResponse::json(
        status,
        serde_json::json!({
            "type": "error",
            "error": {"type": error_type, "message": message}
        }),
    )
}

fn openai_error(status: u16, code: &str, message: &str) -> ProviderResponse {
    ProviderResponse::json(
        status,
        serde_json::json!({
            "error": {"message": message, "type": "invalid_request_error", "code": code}
        }),
    )
}

fn gemini_error(status: u16, google_status: &str, message: &str) -> ProviderResponse {
    ProviderResponse::json(
        status,
        serde_json::json!({
            "error": {"code": status, "message": message, "status": google_status}
        }),
    )
}

#[test]
fn anthropic_error_envelope_maps_to_taxonomy() {
    let codec = anthropic_codec();

    assert!(matches!(
        codec
            .decode_response(anthropic_error(
                401,
                "authentication_error",
                "invalid x-api-key"
            ))
            .unwrap_err(),
        LlmError::Authentication { .. }
    ));
    assert!(matches!(
        codec
            .decode_response(anthropic_error(
                400,
                "invalid_request_error",
                "prompt is too long: 250000 tokens"
            ))
            .unwrap_err(),
        LlmError::ContextOverflow { .. }
    ));
    assert!(matches!(
        codec.decode_response(anthropic_error(400, "invalid_request_error", "messages: roles must alternate")).unwrap_err(),
        LlmError::InvalidRequest { message } if message.contains("roles must alternate")
    ));
    assert!(matches!(
        codec
            .decode_response(anthropic_error(529, "overloaded_error", "Overloaded"))
            .unwrap_err(),
        LlmError::Overloaded { .. }
    ));
}

#[test]
fn anthropic_rate_limit_carries_retry_after_with_ms_precedence() {
    let codec = anthropic_codec();

    let mut seconds_only = anthropic_error(429, "rate_limit_error", "slow down");
    seconds_only
        .headers
        .insert("Retry-After".to_string(), "7".to_string());
    assert!(matches!(
        codec.decode_response(seconds_only).unwrap_err(),
        LlmError::RateLimited { retry_after: Some(after), .. } if after == Duration::from_secs(7)
    ));

    let mut with_ms = anthropic_error(429, "rate_limit_error", "slow down");
    with_ms
        .headers
        .insert("retry-after".to_string(), "7".to_string());
    with_ms
        .headers
        .insert("retry-after-ms".to_string(), "250".to_string());
    assert!(matches!(
        codec.decode_response(with_ms).unwrap_err(),
        LlmError::RateLimited { retry_after: Some(after), .. } if after == Duration::from_millis(250)
    ));
}

#[test]
fn openai_error_envelope_maps_to_taxonomy() {
    let codec = openai_codec();

    assert!(matches!(
        codec
            .decode_response(openai_error(
                401,
                "invalid_api_key",
                "Incorrect API key provided"
            ))
            .unwrap_err(),
        LlmError::Authentication { .. }
    ));
    assert!(matches!(
        codec
            .decode_response(openai_error(
                429,
                "insufficient_quota",
                "You exceeded your current quota"
            ))
            .unwrap_err(),
        LlmError::QuotaExceeded
    ));
    assert!(matches!(
        codec
            .decode_response(openai_error(
                429,
                "rate_limit_exceeded",
                "Rate limit reached"
            ))
            .unwrap_err(),
        LlmError::RateLimited { .. }
    ));
    assert!(matches!(
        codec
            .decode_response(openai_error(
                400,
                "context_length_exceeded",
                "This model's maximum context length is exceeded"
            ))
            .unwrap_err(),
        LlmError::ContextOverflow { .. }
    ));
    assert!(matches!(
        codec.decode_response(openai_error(400, "invalid_value", "Invalid value for tool_choice")).unwrap_err(),
        LlmError::InvalidRequest { message } if message.contains("tool_choice")
    ));
    assert!(matches!(
        codec
            .decode_response(openai_error(500, "server_error", "The server had an error"))
            .unwrap_err(),
        LlmError::ProviderInternal
    ));
}

#[test]
fn gemini_error_envelope_maps_to_taxonomy() {
    let codec = gemini_codec();

    assert!(matches!(
        codec.decode_response(gemini_error(400, "INVALID_ARGUMENT", "Invalid JSON payload")).unwrap_err(),
        LlmError::InvalidRequest { message } if message.contains("Invalid JSON payload")
    ));
    assert!(matches!(
        codec
            .decode_response(gemini_error(401, "UNAUTHENTICATED", "API key not valid"))
            .unwrap_err(),
        LlmError::Authentication { .. }
    ));
    assert!(matches!(
        codec
            .decode_response(gemini_error(403, "PERMISSION_DENIED", "no access"))
            .unwrap_err(),
        LlmError::PermissionDenied { .. }
    ));
    assert!(matches!(
        codec
            .decode_response(gemini_error(404, "NOT_FOUND", "model not found"))
            .unwrap_err(),
        LlmError::ModelUnavailable
    ));
    assert!(matches!(
        codec
            .decode_response(gemini_error(429, "RESOURCE_EXHAUSTED", "quota exceeded"))
            .unwrap_err(),
        LlmError::RateLimited { .. }
    ));
    assert!(matches!(
        codec
            .decode_response(gemini_error(503, "UNAVAILABLE", "service overloaded"))
            .unwrap_err(),
        LlmError::Overloaded { .. }
    ));
}

#[test]
fn error_status_without_envelope_falls_back_to_status_mapping() {
    assert!(matches!(
        anthropic_codec()
            .decode_response(ProviderResponse::json(401, serde_json::Value::Null))
            .unwrap_err(),
        LlmError::Authentication { .. }
    ));
    assert!(matches!(
        openai_codec()
            .decode_response(ProviderResponse::json(
                502,
                serde_json::json!("bad gateway")
            ))
            .unwrap_err(),
        LlmError::ProviderInternal
    ));
    assert!(matches!(
        gemini_codec()
            .decode_response(ProviderResponse::json(429, serde_json::Value::Null))
            .unwrap_err(),
        LlmError::RateLimited { .. }
    ));
}

/// PTL error carries the parsed `token_gap` (actual - limit).
/// Regression test for Fix 1: `LlmError::ContextOverflow { token_gap }`.
#[test]
fn ptl_context_overflow_carries_token_gap() {
    let codec = anthropic_codec();

    // "prompt is too long: 200001 tokens > 200000 maximum" → gap = 1
    let err = codec
        .decode_response(anthropic_error(
            400,
            "invalid_request_error",
            "prompt is too long: 200001 tokens > 200000 maximum",
        ))
        .unwrap_err();
    assert!(
        matches!(err, LlmError::ContextOverflow { token_gap: 1 }),
        "gap should be 1; got {err:?}"
    );

    // Larger gap: 250000 > 200000 → 50000
    let err2 = codec
        .decode_response(anthropic_error(
            400,
            "invalid_request_error",
            "prompt is too long: 250000 tokens > 200000 maximum",
        ))
        .unwrap_err();
    assert!(
        matches!(err2, LlmError::ContextOverflow { token_gap: 50000 }),
        "gap should be 50000; got {err2:?}"
    );

    // PTL message without counts → gap = 0
    let err3 = codec
        .decode_response(anthropic_error(
            400,
            "invalid_request_error",
            "prompt is too long for this model",
        ))
        .unwrap_err();
    assert!(
        matches!(err3, LlmError::ContextOverflow { token_gap: 0 }),
        "gap should be 0 (no counts); got {err3:?}"
    );

    // request_too_large WITHOUT "context window" → RequestTooLarge (2.1.212 413
    // split: accumulated images/attachments, NOT a token overflow).
    let err4 = codec
        .decode_response(anthropic_error(
            413,
            "request_too_large",
            "request too large",
        ))
        .unwrap_err();
    assert!(
        matches!(err4, LlmError::RequestTooLarge),
        "request_too_large (no context window) should be RequestTooLarge; got {err4:?}"
    );

    // request_too_large WHOSE MESSAGE mentions the context window → stays a
    // ContextOverflow (prompt-too-long / compaction path).
    let err5 = codec
        .decode_response(anthropic_error(
            413,
            "request_too_large",
            "prompt exceeds the model's context window",
        ))
        .unwrap_err();
    assert!(
        matches!(err5, LlmError::ContextOverflow { token_gap: 0 }),
        "413 mentioning context window should be ContextOverflow; got {err5:?}"
    );
}

#[test]
fn overloaded_maps_to_dedicated_variant() {
    let codec = anthropic_codec();

    assert!(matches!(
        codec
            .decode_response(anthropic_error(529, "overloaded_error", "Overloaded"))
            .unwrap_err(),
        LlmError::Overloaded { repeated: false }
    ));
    assert!(matches!(
        anthropic_codec()
            .decode_response(ProviderResponse::json(529, serde_json::Value::Null))
            .unwrap_err(),
        LlmError::Overloaded { repeated: false }
    ));
    assert!(matches!(
        codec
            .decode_response(anthropic_error(500, "api_error", "boom"))
            .unwrap_err(),
        LlmError::ProviderInternal
    ));
}
