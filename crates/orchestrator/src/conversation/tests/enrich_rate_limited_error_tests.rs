use super::*;

fn rate_limited() -> LlmError {
    LlmError::RateLimited {
        retry_after: None,
        scope: None,
    }
}

/// A streaming connect-phase 429 (the wrapper `try_run_turn_streaming`
/// produces) re-maps onto the composed copy too.
#[test]
fn streaming_429_with_copy_maps_to_rate_limit_rejected() {
    let err = enrich_rate_limited_error(
        OrchestratorError::Streaming(rate_limited()),
        Some("You've hit your weekly limit · resets 3pm".to_string()),
    );
    assert!(
        matches!(err, OrchestratorError::RateLimitRejected { .. }),
        "got {err:?}"
    );
    assert_eq!(err.to_string(), "You've hit your weekly limit · resets 3pm");
}

/// No composed copy → both wrappers pass through untouched.
#[test]
fn rate_limited_without_copy_passes_through() {
    let api = enrich_rate_limited_error(OrchestratorError::ApiCall(rate_limited()), None);
    assert!(matches!(
        api,
        OrchestratorError::ApiCall(LlmError::RateLimited { .. })
    ));
    let stream = enrich_rate_limited_error(OrchestratorError::Streaming(rate_limited()), None);
    assert!(matches!(
        stream,
        OrchestratorError::Streaming(LlmError::RateLimited { .. })
    ));
}

/// A non-429 error never consults the copy — even when one is cached.
#[test]
fn non_rate_limited_ignores_copy() {
    let err = enrich_rate_limited_error(
        OrchestratorError::StreamEndedWithoutStop,
        Some("You've hit your weekly limit".to_string()),
    );
    assert!(matches!(err, OrchestratorError::StreamEndedWithoutStop));
}
