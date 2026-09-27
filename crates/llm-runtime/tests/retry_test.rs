use std::time::Duration;

use llm_runtime::{LlmError, ResponseMetadata, RetryDecision, RetryPolicy};

// ── Item 1: HTTP-date Retry-After ─────────────────────────────────────────────

#[test]
fn retry_after_imf_fixdate_produces_positive_duration() {
    // A fixdate well in the future must yield Some(duration > 0).
    // Year 9999-01-01 is a Friday — confirmed via Python datetime.
    let metadata =
        ResponseMetadata::new(429).with_header("retry-after", "Fri, 01 Jan 9999 00:00:00 GMT");
    let decision = RetryPolicy.classify_response(&metadata);
    // Must be Retry with a non-None duration.
    match decision {
        RetryDecision::Retry { after: Some(d) } => assert!(d.as_secs() > 0),
        other => panic!("expected Retry with duration, got {other:?}"),
    }
}

#[test]
fn retry_after_past_imf_fixdate_clamps_to_zero() {
    // A fixdate in the past must yield Some(Duration::ZERO) (past → 0).
    // 2001-01-01 is a Monday — confirmed via Python datetime.
    let metadata =
        ResponseMetadata::new(429).with_header("retry-after", "Mon, 01 Jan 2001 00:00:00 GMT");
    let decision = RetryPolicy.classify_response(&metadata);
    assert_eq!(
        decision,
        RetryDecision::Retry {
            after: Some(Duration::ZERO)
        }
    );
}

#[test]
fn retry_after_garbage_string_yields_none() {
    let metadata = ResponseMetadata::new(429).with_header("retry-after", "not-a-date-or-number");
    let decision = RetryPolicy.classify_response(&metadata);
    assert_eq!(decision, RetryDecision::Retry { after: None });
}

#[test]
fn retry_after_ms_still_takes_precedence_over_fixdate() {
    let metadata = ResponseMetadata::new(429)
        .with_header("retry-after", "Fri, 01 Jan 9999 00:00:00 GMT")
        .with_header("retry-after-ms", "250");
    let decision = RetryPolicy.classify_response(&metadata);
    assert_eq!(
        decision,
        RetryDecision::Retry {
            after: Some(Duration::from_millis(250))
        }
    );
}

#[test]
fn retryable_response_statuses_are_retried() {
    let policy = RetryPolicy;

    for status in [429, 500, 502, 503, 504, 529] {
        let decision = policy.classify_response(&ResponseMetadata::new(status));
        assert_eq!(
            decision,
            RetryDecision::Retry { after: None },
            "status {status}"
        );
    }
}

#[test]
fn retry_after_header_seconds_are_preserved() {
    let metadata = ResponseMetadata::new(429).with_header("retry-after", "7");

    let decision = RetryPolicy.classify_response(&metadata);

    assert_eq!(
        decision,
        RetryDecision::Retry {
            after: Some(Duration::from_secs(7))
        }
    );
}

#[test]
fn retry_after_ms_header_takes_precedence_over_seconds() {
    let metadata = ResponseMetadata::new(429)
        .with_header("retry-after", "7")
        .with_header("retry-after-ms", "250");

    let decision = RetryPolicy.classify_response(&metadata);

    assert_eq!(
        decision,
        RetryDecision::Retry {
            after: Some(Duration::from_millis(250))
        }
    );
}

#[test]
fn non_retryable_errors_do_not_retry() {
    let policy = RetryPolicy;
    let errors = [
        LlmError::Authentication {
            message: String::new(),
        },
        LlmError::PermissionDenied {
            message: String::new(),
        },
        LlmError::InvalidRequest {
            message: "bad".to_string(),
        },
        LlmError::ContextOverflow { token_gap: 0 },
        // 2.1.212 413 request-too-large is terminal: compaction cannot shed the
        // image/attachment bytes, so retrying only burns the budget.
        LlmError::RequestTooLarge,
        LlmError::UnsupportedCapability {
            capability: "vision".to_string(),
        },
        LlmError::CostUnavailable {
            message: "missing".to_string(),
        },
    ];

    for error in errors {
        assert_eq!(policy.classify_error(&error), RetryDecision::DoNotRetry);
    }
}

#[test]
fn stream_interruption_after_events_is_not_replayed() {
    let error = LlmError::StreamInterrupted {
        message: "socket closed".to_string(),
    };

    assert_eq!(
        RetryPolicy.classify_error(&error),
        RetryDecision::DoNotRetry
    );
}

#[test]
fn overloaded_errors_are_retryable() {
    assert_eq!(
        RetryPolicy.classify_error(&LlmError::Overloaded { repeated: false }),
        RetryDecision::Retry { after: None }
    );
}
