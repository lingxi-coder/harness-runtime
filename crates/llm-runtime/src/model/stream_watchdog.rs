//! Streaming idle watchdog (cc 2.1.196 default-on).
//!
//! Ports the 2.1.198 binary's stream-idle watchdog resolution:
//!
//! ```js
//! // query loop @219636542:
//! jo = Oe.CLAUDE_ENABLE_STREAM_WATCHDOG ?? !0          // default ON
//! ss = fzr()                                           // event-idle timeout
//! // fzr @208691984:
//! function fzr(){ return Math.max(Number(process.env.CLAUDE_STREAM_IDLE_TIMEOUT_MS)||0, 300000) }
//! ```
//!
//! LingXi accepts BOTH the `LINGXI_*` and `CLAUDE_*` spellings of the two env
//! knobs (LINGXI takes precedence), per the branding-alias convention:
//!
//! - `LINGXI_ENABLE_STREAM_WATCHDOG` / `CLAUDE_ENABLE_STREAM_WATCHDOG` —
//!   `0`/`false` (any case) disables; absent or anything else keeps the
//!   default ON.
//! - `LINGXI_STREAM_IDLE_TIMEOUT_MS` / `CLAUDE_STREAM_IDLE_TIMEOUT_MS` —
//!   raises the idle timeout; values below the 5-minute floor are clamped up
//!   (binary `Math.max(n||0, 300000)` — the floor also swallows unparseable
//!   or non-positive values).
//!
//! When the watchdog fires, the stream is aborted with
//! [`crate::LlmError::StreamInterrupted`] whose message starts with
//! [`STREAM_IDLE_TIMEOUT_PREFIX`] — callers (the orchestrator turn loop)
//! detect that prefix via [`is_stream_idle_timeout`] to drive the 2.1.198
//! watchdog-retry rules (retry once after a thinking-only yield; finalize the
//! partial response otherwise).

use std::time::Duration;

/// The 5-minute idle floor: binary `fzr()`'s `300000` ms.
pub const STREAM_IDLE_TIMEOUT_FLOOR_MS: u64 = 300_000;

/// Minimum first-byte window accepted by the oracle (`tmt = 1e4`).
pub const STREAM_FIRST_BYTE_TIMEOUT_MIN_MS: u64 = 10_000;

/// Maximum explicitly configured first-byte window (`SEn = 1_800_000`).
pub const STREAM_FIRST_BYTE_TIMEOUT_MAX_MS: u64 = 1_800_000;

/// Default first-party first-byte window (`YMo = 180_000`).
pub const STREAM_FIRST_BYTE_TIMEOUT_FIRST_PARTY_MS: u64 = 180_000;

/// Request-body chunk size used by the first-byte allowance (`JMo = 32_768`).
pub const STREAM_FIRST_BYTE_BODY_CHUNK_BYTES: usize = 32_768;

/// Extra first-byte allowance per request-body chunk (`QMo = 1_000`).
pub const STREAM_FIRST_BYTE_BODY_CHUNK_MS: u64 = 1_000;

/// Default overall API timeout used to cap the first-byte watchdog.
pub const API_TIMEOUT_DEFAULT_MS: u64 = 600_000;

/// Marker message carried by the oracle's `StreamNoResponseError`.
pub const STREAM_NO_RESPONSE_MESSAGE: &str = "No response from API within the first-byte window";

/// Marker prefix for watchdog-aborted streams. The binary's synthesized
/// errors use the same prefix (`"Stream idle timeout - partial response
/// received"` / `"Stream idle timeout - no chunks received"` @219649648).
pub const STREAM_IDLE_TIMEOUT_PREFIX: &str = "Stream idle timeout";

/// Marker prefix for a watchdog abort caused by the machine SUSPENDING.
///
/// The oracle raises a separate error class for this — `StreamSuspendedError`
/// (`code = "StreamSuspended"`, carrying `sleptMs`), whose message reads
/// "Stream watchdog detected system suspend; aborting to retry on a fresh
/// connection" (@228830985). It is deliberately NOT the idle timeout: a stream
/// that stalled because the laptop slept is not a stalled server, and `sir()`
/// tells the user so.
///
/// Follows the same prefix idiom as [`STREAM_IDLE_TIMEOUT_PREFIX`] rather than
/// adding an `LlmError` variant — this is our own marker on our own message,
/// not an inference about a provider's wording.
pub const STREAM_SUSPENDED_PREFIX: &str = "Stream watchdog detected system suspend";

/// How much wall-clock drift past the monotonic timeout counts as a suspend.
///
/// During a real suspend the monotonic clock stops while the wall clock keeps
/// running, so the gap is the sleep duration. A second of slack keeps ordinary
/// scheduling jitter and NTP nudges out of the branch.
const SUSPEND_DRIFT_FLOOR: Duration = Duration::from_secs(1);

/// Classify a fired idle timeout: did the machine sleep through it?
///
/// `wall_elapsed` is measured with the system clock across the same wait the
/// monotonic `timeout` bounded. Monotonic time does not advance while suspended,
/// so a wall-clock excess beyond [`SUSPEND_DRIFT_FLOOR`] is the sleep.
#[must_use]
pub fn watchdog_abort_error(timeout: Duration, wall_elapsed: Duration) -> crate::LlmError {
    let slept = wall_elapsed.saturating_sub(timeout);
    if slept >= SUSPEND_DRIFT_FLOOR {
        return crate::LlmError::StreamInterrupted {
            message: format!(
                "{STREAM_SUSPENDED_PREFIX}; aborting to retry on a fresh connection \
                 (slept {}ms)",
                slept.as_millis()
            ),
        };
    }
    idle_timeout_error(timeout)
}

/// Whether an [`crate::LlmError`] is a watchdog SUSPEND abort.
#[must_use]
pub fn is_stream_suspended(error: &crate::LlmError) -> bool {
    matches!(
        error,
        crate::LlmError::StreamInterrupted { message } if message.starts_with(STREAM_SUSPENDED_PREFIX)
    )
}

/// Whether an [`crate::LlmError`] is a watchdog idle-timeout abort.
#[must_use]
pub fn is_stream_idle_timeout(error: &crate::LlmError) -> bool {
    matches!(
        error,
        crate::LlmError::StreamInterrupted { message } if message.starts_with(STREAM_IDLE_TIMEOUT_PREFIX)
    )
}

/// Build the watchdog abort error for a given configured timeout.
#[must_use]
pub fn idle_timeout_error(timeout: Duration) -> crate::LlmError {
    crate::LlmError::StreamInterrupted {
        message: format!(
            "{STREAM_IDLE_TIMEOUT_PREFIX} - no stream event within {}ms",
            timeout.as_millis()
        ),
    }
}

/// Build the connect-phase timeout error corresponding to the oracle's
/// `StreamNoResponseError` (`code = "StreamNoResponse"`).
#[must_use]
pub fn first_byte_timeout_error() -> crate::LlmError {
    crate::LlmError::Transport {
        message: STREAM_NO_RESPONSE_MESSAGE.to_string(),
    }
}

/// Classify a fired first-byte timer as an ordinary no-response timeout or a
/// machine suspend. The oracle compares accumulated event-loop sleep with half
/// the armed window before choosing `StreamSuspended`.
#[must_use]
pub fn first_byte_abort_error(timeout: Duration, wall_elapsed: Duration) -> crate::LlmError {
    let slept = wall_elapsed.saturating_sub(timeout);
    if slept > timeout / 2 {
        watchdog_abort_error(timeout, wall_elapsed)
    } else {
        first_byte_timeout_error()
    }
}

/// Whether an error is the locally constructed `StreamNoResponse` marker.
#[must_use]
pub fn is_stream_no_response(error: &crate::LlmError) -> bool {
    matches!(
        error,
        crate::LlmError::Transport { message } if message == STREAM_NO_RESPONSE_MESSAGE
    )
}

/// `Oe.CLAUDE_ENABLE_STREAM_WATCHDOG ?? !0` — enabled unless the env value is
/// explicitly falsy (`0` / `false`, case-insensitive). `lingxi` wins over
/// `claude` when both are set.
#[must_use]
pub fn watchdog_enabled_from_values(lingxi: Option<&str>, claude: Option<&str>) -> bool {
    let raw = lingxi.or(claude);
    match raw {
        Some(v) => {
            let v = v.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false"))
        }
        None => true,
    }
}

/// Binary `fzr()`: `Math.max(Number(env)||0, 300000)`. Unparseable / missing
/// / non-positive values fall to the floor. `lingxi` wins over `claude`.
#[must_use]
pub fn idle_timeout_from_values(lingxi: Option<&str>, claude: Option<&str>) -> Duration {
    let parsed: u64 = lingxi
        .or(claude)
        .and_then(parse_js_number_millis)
        .unwrap_or(0);
    Duration::from_millis(parsed.max(STREAM_IDLE_TIMEOUT_FLOOR_MS))
}

fn parse_js_number_millis(raw: &str) -> Option<u64> {
    let parsed = raw.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed <= 0.0 {
        return None;
    }
    if parsed >= u64::MAX as f64 {
        return Some(u64::MAX);
    }
    Some(parsed.trunc() as u64)
}

fn parse_js_integer_millis(raw: &str) -> Option<i128> {
    let parsed = raw.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    if parsed >= i128::MAX as f64 {
        return Some(i128::MAX);
    }
    if parsed <= i128::MIN as f64 {
        return Some(i128::MIN);
    }
    Some(parsed.trunc() as i128)
}

fn positive_millis(raw: Option<&str>) -> Option<u64> {
    let parsed = parse_js_integer_millis(raw?)?;
    u64::try_from(parsed).ok().filter(|value| *value > 0)
}

/// Resolve the byte-level idle base used by the first-byte watchdog.
///
/// This is the oracle's `ZMo(provider)`: an explicit byte-idle value wins;
/// otherwise an explicit legacy stream-idle value is honored, and only the
/// absent-env first-party default drops from five minutes to three minutes.
#[must_use]
pub fn byte_stream_idle_timeout_from_values(
    stream_idle: Option<&str>,
    byte_stream_idle: Option<&str>,
    first_party: bool,
) -> Duration {
    let stream_idle_explicit = positive_millis(stream_idle);
    let legacy_idle = stream_idle_explicit
        .unwrap_or(0)
        .max(STREAM_IDLE_TIMEOUT_FLOOR_MS);
    let provider_default = if first_party {
        STREAM_FIRST_BYTE_TIMEOUT_FIRST_PARTY_MS
    } else {
        legacy_idle
    };
    let resolved = positive_millis(byte_stream_idle).unwrap_or_else(|| {
        if stream_idle_explicit.is_some() {
            legacy_idle
        } else {
            provider_default
        }
    });
    Duration::from_millis(resolved.clamp(
        STREAM_FIRST_BYTE_TIMEOUT_MIN_MS,
        STREAM_FIRST_BYTE_TIMEOUT_MAX_MS,
    ))
}

/// Resolve the connect-phase first-byte window from already captured values.
///
/// Mirrors the oracle's `XMo` + `nOo` composition, including its two distinct
/// `API_TIMEOUT_MS` defaults: zero while choosing the base, then 600 seconds
/// while applying the final cap. `None` means the API timeout leaves less than
/// the oracle's ten-second minimum, so the first-byte watchdog is not armed.
#[must_use]
pub fn first_byte_timeout_from_values(
    first_byte: Option<&str>,
    stream_idle: Option<&str>,
    byte_stream_idle: Option<&str>,
    api_timeout: Option<&str>,
    first_party: bool,
    body_bytes: usize,
) -> Option<Duration> {
    let explicit_first_byte = positive_millis(first_byte).map(|value| {
        value.clamp(
            STREAM_FIRST_BYTE_TIMEOUT_MIN_MS,
            STREAM_FIRST_BYTE_TIMEOUT_MAX_MS,
        )
    });
    let base = explicit_first_byte.unwrap_or_else(|| {
        let byte_idle = u64::try_from(
            byte_stream_idle_timeout_from_values(stream_idle, byte_stream_idle, first_party)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let api_base = parse_js_integer_millis(api_timeout.unwrap_or("0"))
            .unwrap_or(0)
            .saturating_sub(1_000);
        u64::try_from(api_base)
            .ok()
            .filter(|value| *value > byte_idle)
            .unwrap_or(byte_idle)
    });

    let body_chunks = body_bytes.saturating_add(STREAM_FIRST_BYTE_BODY_CHUNK_BYTES - 1)
        / STREAM_FIRST_BYTE_BODY_CHUNK_BYTES;
    let body_allowance = u64::try_from(body_chunks)
        .unwrap_or(u64::MAX)
        .saturating_mul(STREAM_FIRST_BYTE_BODY_CHUNK_MS);
    let requested = base.saturating_add(body_allowance);

    let api_timeout = api_timeout
        .and_then(parse_js_integer_millis)
        .unwrap_or(i128::from(API_TIMEOUT_DEFAULT_MS));
    if api_timeout <= 0 {
        return Some(Duration::from_millis(requested));
    }
    let cap = api_timeout.saturating_sub(1_000);
    if cap < i128::from(STREAM_FIRST_BYTE_TIMEOUT_MIN_MS) {
        return None;
    }
    let cap = u64::try_from(cap).unwrap_or(u64::MAX);
    Some(Duration::from_millis(requested.min(cap)))
}

fn env_value<'a>(lingxi: &'a str, claude: &'a str) -> Option<String> {
    std::env::var(lingxi)
        .ok()
        .or_else(|| std::env::var(claude).ok())
}

/// Resolve the effective first-byte timeout for the current provider/request.
///
/// Claude enables this by default for first-party Anthropic traffic. Bedrock's
/// binary stream remains opt-in; unrelated/custom providers are not armed.
#[must_use]
pub fn resolve_stream_first_byte_timeout(
    provider: &crate::ProviderId,
    body_bytes: usize,
) -> Option<Duration> {
    let byte_watchdog_enabled = watchdog_enabled_from_values(
        std::env::var("LINGXI_ENABLE_BYTE_WATCHDOG").ok().as_deref(),
        std::env::var("CLAUDE_ENABLE_BYTE_WATCHDOG").ok().as_deref(),
    );
    if !byte_watchdog_enabled {
        return None;
    }

    let first_party = matches!(provider, crate::ProviderId::AnthropicFirstParty);
    let bedrock_setting = env_value(
        "LINGXI_ENABLE_BYTE_WATCHDOG_BEDROCK",
        "CLAUDE_ENABLE_BYTE_WATCHDOG_BEDROCK",
    );
    let bedrock_enabled = matches!(provider, crate::ProviderId::BedrockClaude)
        && bedrock_setting
            .as_deref()
            .is_some_and(|value| watchdog_enabled_from_values(Some(value), None));
    if !first_party && !bedrock_enabled {
        return None;
    }

    let first_byte = env_value(
        "LINGXI_STREAM_FIRST_BYTE_TIMEOUT_MS",
        "CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS",
    );
    let stream_idle = env_value(
        "LINGXI_STREAM_IDLE_TIMEOUT_MS",
        "CLAUDE_STREAM_IDLE_TIMEOUT_MS",
    );
    let byte_stream_idle = env_value(
        "LINGXI_BYTE_STREAM_IDLE_TIMEOUT_MS",
        "CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS",
    );
    first_byte_timeout_from_values(
        first_byte.as_deref(),
        stream_idle.as_deref(),
        byte_stream_idle.as_deref(),
        std::env::var("API_TIMEOUT_MS").ok().as_deref(),
        first_party,
        body_bytes,
    )
}

/// Resolve the effective idle timeout from the process environment:
/// `Some(timeout)` when the watchdog is enabled (the default), `None` when
/// explicitly disabled via `LINGXI_ENABLE_STREAM_WATCHDOG=0` (or the
/// `CLAUDE_` alias).
#[must_use]
pub fn resolve_stream_idle_timeout() -> Option<Duration> {
    let enabled = watchdog_enabled_from_values(
        std::env::var("LINGXI_ENABLE_STREAM_WATCHDOG")
            .ok()
            .as_deref(),
        std::env::var("CLAUDE_ENABLE_STREAM_WATCHDOG")
            .ok()
            .as_deref(),
    );
    if !enabled {
        return None;
    }
    Some(idle_timeout_from_values(
        std::env::var("LINGXI_STREAM_IDLE_TIMEOUT_MS")
            .ok()
            .as_deref(),
        std::env::var("CLAUDE_STREAM_IDLE_TIMEOUT_MS")
            .ok()
            .as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled_with_five_minute_floor() {
        // Binary: `jo = Oe.CLAUDE_ENABLE_STREAM_WATCHDOG ?? !0` (default ON),
        // `fzr()` floors at 300000ms.
        assert!(watchdog_enabled_from_values(None, None));
        assert_eq!(
            idle_timeout_from_values(None, None),
            Duration::from_millis(300_000)
        );
    }

    #[test]
    fn zero_or_false_disables_via_either_spelling() {
        assert!(!watchdog_enabled_from_values(Some("0"), None));
        assert!(!watchdog_enabled_from_values(None, Some("0")));
        assert!(!watchdog_enabled_from_values(Some("false"), None));
        assert!(!watchdog_enabled_from_values(None, Some("FALSE")));
        // Any other value (including "1"/"true"/junk) keeps it on.
        assert!(watchdog_enabled_from_values(Some("1"), None));
        assert!(watchdog_enabled_from_values(Some("yes"), None));
    }

    #[test]
    fn lingxi_spelling_wins_over_claude() {
        // LINGXI=1 overrides CLAUDE=0.
        assert!(watchdog_enabled_from_values(Some("1"), Some("0")));
        // LINGXI=0 overrides CLAUDE=1.
        assert!(!watchdog_enabled_from_values(Some("0"), Some("1")));
        assert_eq!(
            idle_timeout_from_values(Some("600000"), Some("900000")),
            Duration::from_millis(600_000)
        );
    }

    #[test]
    fn timeout_below_floor_clamps_up_and_junk_falls_to_floor() {
        // Binary `Math.max(Number(v)||0, 300000)`.
        assert_eq!(
            idle_timeout_from_values(Some("1000"), None),
            Duration::from_millis(300_000)
        );
        assert_eq!(
            idle_timeout_from_values(Some("junk"), None),
            Duration::from_millis(300_000)
        );
        assert_eq!(
            idle_timeout_from_values(Some("600000"), None),
            Duration::from_millis(600_000)
        );
        assert_eq!(
            idle_timeout_from_values(Some("1e6"), None),
            Duration::from_millis(1_000_000)
        );
        assert_eq!(
            idle_timeout_from_values(Some("2.5e5"), None),
            Duration::from_millis(300_000)
        );
        assert_eq!(
            idle_timeout_from_values(Some("600000.9"), None),
            Duration::from_millis(600_000)
        );
    }

    #[test]
    fn idle_timeout_error_is_detectable() {
        let err = idle_timeout_error(Duration::from_millis(300_000));
        assert!(is_stream_idle_timeout(&err));
        assert!(!is_stream_idle_timeout(&crate::LlmError::Transport {
            message: "connection reset".into()
        }));
        assert!(!is_stream_idle_timeout(
            &crate::LlmError::StreamInterrupted {
                message: "malformed frame".into()
            }
        ));
    }

    #[test]
    fn first_byte_defaults_and_body_allowance_match_oracle() {
        assert_eq!(
            first_byte_timeout_from_values(None, None, None, None, true, 0),
            Some(Duration::from_millis(180_000))
        );
        assert_eq!(
            first_byte_timeout_from_values(None, None, None, None, false, 0),
            Some(Duration::from_millis(300_000))
        );
        assert_eq!(
            first_byte_timeout_from_values(None, None, None, None, true, 32_769),
            Some(Duration::from_millis(182_000))
        );
    }

    #[test]
    fn first_byte_explicit_values_and_legacy_idle_are_clamped_exactly() {
        assert_eq!(
            first_byte_timeout_from_values(Some("1"), None, None, None, true, 0),
            Some(Duration::from_millis(10_000))
        );
        assert_eq!(
            first_byte_timeout_from_values(Some("9999999"), None, None, Some("0"), true, 0,),
            Some(Duration::from_millis(1_800_000))
        );
        assert_eq!(
            first_byte_timeout_from_values(None, Some("600000"), None, None, true, 0),
            Some(Duration::from_millis(599_000))
        );
        assert_eq!(
            first_byte_timeout_from_values(None, None, Some("450000"), None, true, 0),
            Some(Duration::from_millis(450_000))
        );
    }

    #[test]
    fn api_timeout_reserves_one_second_or_disables_too_small_window() {
        // An explicitly configured API timeout becomes the effective cap.
        assert_eq!(
            first_byte_timeout_from_values(None, None, None, Some("120000"), true, 0),
            Some(Duration::from_millis(119_000))
        );
        // Below the oracle's 10-second minimum, nOo does not arm the timer.
        assert_eq!(
            first_byte_timeout_from_values(None, None, None, Some("10000"), true, 0),
            None
        );
        // A non-positive API timeout disables only the overall cap.
        assert_eq!(
            first_byte_timeout_from_values(Some("1800000"), None, None, Some("0"), true, 32_768,),
            Some(Duration::from_millis(1_801_000))
        );
    }

    #[test]
    fn first_byte_timeout_error_has_exact_marker() {
        let error = first_byte_timeout_error();
        assert!(is_stream_no_response(&error));
        assert_eq!(error.provider_message(), Some(STREAM_NO_RESPONSE_MESSAGE));
        assert!(!is_stream_no_response(&crate::LlmError::Transport {
            message: "Connection error.".to_string(),
        }));

        let timeout = Duration::from_secs(180);
        assert!(is_stream_no_response(&first_byte_abort_error(
            timeout,
            Duration::from_secs(200),
        )));
        assert!(is_stream_suspended(&first_byte_abort_error(
            timeout,
            Duration::from_secs(300),
        )));
    }
}
