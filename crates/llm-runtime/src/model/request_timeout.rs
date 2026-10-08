//! Current non-streaming timeout selection and request-option validation.
use crate::LlmError;
use std::time::Duration;

pub(crate) fn non_stream_timeout_from_values(
    api_timeout: Option<&str>,
    remote: Option<&str>,
) -> Result<Duration, LlmError> {
    validate_timeout(selected_millis(api_timeout, remote))
}

fn selected_millis(api_timeout: Option<&str>, remote: Option<&str>) -> f64 {
    let value = api_timeout.map_or(f64::NAN, lingxi_core::host::env::parse_int_env);
    if !value.is_nan() && value != 0.0 {
        value.min(f64::from(i32::MAX))
    } else if remote.is_some_and(crate::structured_output::bool_value) {
        120_000.0
    } else {
        300_000.0
    }
}

fn validate_timeout(millis: f64) -> Result<Duration, LlmError> {
    // Selection uses numeric truthiness, including negative overrides. Native
    // SDK request validation rejects negatives before a physical dispatch.
    if !millis.is_finite() || millis.fract() != 0.0 {
        return Err(LlmError::InvalidRequest {
            message: "timeout must be an integer".into(),
        });
    }
    if millis < 0.0 {
        return Err(LlmError::InvalidRequest {
            message: "timeout must be a positive integer".into(),
        });
    }
    Ok(Duration::from_millis(millis as u64))
}

/// Current nonstream request deadline. Capture once for a nonstream drive.
pub fn non_stream_timeout() -> Result<Duration, LlmError> {
    non_stream_timeout_from_values(
        std::env::var("API_TIMEOUT_MS").ok().as_deref(),
        std::env::var(branding::REMOTE_ENV).ok().as_deref(),
    )
}

/// Current stream-failure comparison. Native uzt is read again when the stream
/// fails; SDK validation belongs to the later nonstream request, not this test.
#[must_use]
pub fn stream_outlasted_nonstream_timeout(elapsed: Duration) -> bool {
    let threshold = selected_millis(
        std::env::var("API_TIMEOUT_MS").ok().as_deref(),
        std::env::var(branding::REMOTE_ENV).ok().as_deref(),
    );
    elapsed.as_millis() as f64 >= threshold
}

fn timeout_retry_limit(explicit: Option<&str>, persistent: bool, long_stream: bool) -> Option<f64> {
    let explicit = explicit.and_then(|value| {
        let value = lingxi_core::host::effort::trim_js_whitespace(value);
        let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value = lingxi_core::host::env::parse_int_env(value);
        (value.is_finite() && value >= 0.0).then_some(value)
    });
    explicit.or_else(|| (persistent && long_stream).then_some(2.0))
}

#[derive(Debug, Default)]
pub(crate) struct TimeoutRetryCount {
    count: u32,
}
impl TimeoutRetryCount {
    pub(crate) fn exhausted_from_environment(
        &mut self,
        failed_stream_outlasted_timeout: bool,
        error: &LlmError,
        timeout: Duration,
        elapsed: Duration,
    ) -> bool {
        self.exhausted(
            std::env::var(branding::NONSTREAMING_TIMEOUT_RETRIES_ENV)
                .ok()
                .as_deref(),
            super::retry::retry_watchdog_from_env(),
            failed_stream_outlasted_timeout,
            matches!(error, LlmError::TransportTimeout { .. }),
            timeout,
            elapsed,
        )
    }

    fn exhausted(
        &mut self,
        explicit: Option<&str>,
        persistent: bool,
        long_stream: bool,
        transport_timeout: bool,
        timeout: Duration,
        elapsed: Duration,
    ) -> bool {
        let limit = timeout_retry_limit(explicit, persistent, long_stream);
        if let Some(limit) = limit {
            if transport_timeout && elapsed.as_millis() as f64 >= timeout.as_millis() as f64 * 0.9 {
                if f64::from(self.count) >= limit {
                    return true;
                }
                self.count = self.count.saturating_add(1);
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_retry_guards_match_current_native_schema_duration_and_counts() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/nonstream_timeout_retries_2_1_288.json"
        ))
        .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 16000);
        for case in cases {
            let explicit = case["explicit"].as_str();
            let persistent = case["persistent"].as_bool().unwrap();
            let long_stream = case["long_stream"].as_bool().unwrap();
            assert_eq!(
                timeout_retry_limit(explicit, persistent, long_stream),
                case["limit"].as_f64(),
                "{case}"
            );
            let mut guard = TimeoutRetryCount {
                count: case["count"].as_u64().unwrap() as u32,
            };
            let exhausted = guard.exhausted(
                explicit,
                persistent,
                long_stream,
                case["transport_timeout"].as_bool().unwrap(),
                Duration::from_millis(case["timeout_ms"].as_u64().unwrap()),
                Duration::from_millis(case["elapsed_ms"].as_u64().unwrap()),
            );
            assert_eq!(
                serde_json::json!({"exhausted": exhausted, "count": guard.count}),
                case["expected"],
                "{case}"
            );
        }
    }

    #[test]
    fn stream_duration_authority_never_enters_or_restores_from_model_input() {
        let mut request = crate::LlmRequest::new("fixture");
        request.execution.failed_stream_outlasted_timeout = true;
        let mut serialized = serde_json::to_value(&request).unwrap();
        assert!(serialized.get("execution").is_none());
        serialized["execution"] = serde_json::json!({"failed_stream_outlasted_timeout": true});
        let restored: crate::LlmRequest = serde_json::from_value(serialized).unwrap();
        assert!(!restored.execution.failed_stream_outlasted_timeout);
    }

    #[test]
    fn current_nonstream_timeouts_match_native_selection_and_sdk_validation() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/nonstream_timeout_2_1_288.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let timeout = case["api_timeout"].as_str();
            let remote = case["remote"].as_str();
            let result = non_stream_timeout_from_values(timeout, remote);
            if let Some(error) = case["expected"]["error"].as_str() {
                assert!(
                    matches!(result, Err(LlmError::InvalidRequest { message }) if message == error),
                    "{case}"
                );
            } else {
                assert_eq!(
                    result.unwrap().as_millis(),
                    u128::from(case["expected"]["millis"].as_u64().unwrap()),
                    "{case}"
                );
            }
            let parsed = timeout.map_or(f64::NAN, lingxi_core::host::env::parse_int_env);
            match case["parsed"].as_str() {
                Some("NaN") => assert!(parsed.is_nan(), "{case}"),
                Some("Infinity") => assert_eq!(parsed, f64::INFINITY, "{case}"),
                Some("-Infinity") => assert_eq!(parsed, f64::NEG_INFINITY, "{case}"),
                _ => assert_eq!(parsed, case["parsed"].as_f64().unwrap(), "{case}"),
            }
        }
    }
}
