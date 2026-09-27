//! Provider-neutral error taxonomy.

use std::time::Duration;

/// Public provider-neutral error type for LLM client operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// Authentication failed or credentials are missing/invalid.
    ///
    /// `message` preserves the PROVIDER's own text (with the SDK status prefix,
    /// see `providers::api_error_message`). It exists because the whole
    /// user-facing auth-copy family gates on that text — "OAuth token has been
    /// revoked", "api key authentication is disabled", "x-api-key",
    /// "OAuth authentication is currently not allowed for this organization".
    /// While this was a UNIT variant the decoder dropped the message at the
    /// provider boundary, so every one of those branches was unreachable and
    /// the user got the bare `Display` instead.
    ///
    /// `Display` deliberately stays the fixed string: consumers that render or
    /// match on it are unaffected. Read the provider text via
    /// [`LlmError::provider_message`].
    #[error("authentication failed")]
    Authentication {
        /// Provider error text, empty when constructed internally.
        message: String,
    },
    /// The stored OAuth refresh token was REJECTED by the IdP — the session is
    /// dead and only a fresh `/login` can revive it.
    ///
    /// Parity: claude-code models this as a distinct error class
    /// (`OAuthRefreshDeadError`, minified `qQt`) rather than a flavour of
    /// [`Self::Authentication`], and renders it with its own copy ("Login
    /// expired · Please run /login"). Kept a separate variant here for the same
    /// reason: a stale token hash or an unreachable IdP are also refresh
    /// failures, but neither means the user has to log in again.
    #[error("oauth session expired")]
    OAuthRefreshDead,
    /// Caller is authenticated but not allowed to perform the request.
    ///
    /// Carries the provider's text for the same reason as
    /// [`Self::Authentication`]; `Display` is unchanged.
    #[error("permission denied")]
    PermissionDenied {
        /// Provider error text, empty when constructed internally.
        message: String,
    },
    /// Provider rejected the request as invalid.
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// Provider or validation message explaining why the request is invalid.
        message: String,
    },
    /// Provider rate-limited the request.
    #[error("rate limited")]
    RateLimited {
        /// Optional server-provided retry-after duration.
        retry_after: Option<Duration>,
        /// Optional provider-specific rate-limit scope.
        scope: Option<String>,
    },
    /// Provider quota or billing limit was exceeded.
    #[error("quota exceeded")]
    QuotaExceeded,
    /// Request exceeded provider/model context limits.
    ///
    /// `token_gap` is the actual-minus-limit token gap when the provider
    /// reported counts in the error message; `0` when unknown.
    #[error("context overflow")]
    ContextOverflow {
        /// How many tokens over the limit the prompt was, or `0` when unknown.
        token_gap: u64,
    },
    /// Request body exceeded the provider's maximum size — a 413 whose message
    /// does NOT mention the context window (accumulated images/attachments
    /// pushed the raw request over the byte limit, not token overflow).
    ///
    /// Parity: claude-code 2.1.212 splits status 413 — a message containing
    /// `"context window"` stays prompt-too-long ([`LlmError::ContextOverflow`],
    /// which drives compaction); everything else becomes this DISTINCT variant,
    /// surfaced with the byte-exact `"Request too large (max 32MB). Accumulated
    /// images and attachments…"` notice. Terminal: compaction cannot shed image
    /// bytes, so it is never retried and never triggers the PTL recovery loop.
    #[error("request too large")]
    RequestTooLarge,
    /// Requested model is unavailable.
    #[error("model unavailable")]
    ModelUnavailable,
    /// Provider returned a transient/internal failure.
    #[error("provider internal error")]
    ProviderInternal,
    /// Provider reported overload (Anthropic 529 / `overloaded_error`).
    ///
    /// `repeated` is `true` when the caller has seen `consecutive_overloaded >=
    /// MAX_529_RETRIES` with no fallback available — the orchestrator sets this
    /// flag so upper layers can surface the byte-locked
    /// `"Repeated 529 Overloaded errors"` copy (`errors.ts:166`).  All sources
    /// that decode a fresh 529 from the wire set `repeated: false`.
    #[error("provider overloaded")]
    Overloaded {
        /// `true` when this is the repeated-overload terminal error; `false`
        /// for a fresh 529 from the wire.
        repeated: bool,
    },
    /// Transport failed before a provider response was decoded.
    #[error("transport error: {message}")]
    Transport {
        /// Transport-layer failure message.
        message: String,
    },
    /// The request did not complete within the timeout.
    ///
    /// Split from [`Self::Transport`] because the oracle's `x2()` cause-chain
    /// yields a distinct `ETIMEDOUT` code and `sir()` renders distinct text for
    /// it. `HttpError::Timeout` is already a typed variant here, so keeping the
    /// distinction costs nothing and avoids recovering it by scanning message
    /// text — a predicate the oracle does not have.
    ///
    /// Retry/telemetry classification treats this exactly like `Transport`.
    #[error("transport error: {message}")]
    TransportTimeout {
        /// Transport-layer failure message.
        message: String,
    },
    /// Transport failed due to a TLS/SSL certificate error.
    ///
    /// A certificate failure (expired cert, self-signed cert, a corporate
    /// TLS-intercepting proxy, a protocol/handshake fault, …) is **terminal**:
    /// it is never retried, because retrying a handshake that can't succeed
    /// only burns the retry budget. This is split out from
    /// [`LlmError::Transport`] so retry classification can short-circuit it and
    /// callers can surface the fix hint.
    ///
    /// Parity: claude-code 2.1.201 `JF` cause-chain classifier + the `Gyo`/`bBp`
    /// code sets + the `YLe` hint (see [`crate::ssl`]). `code` is the matched
    /// Node/OpenSSL error code (e.g. `CERT_HAS_EXPIRED`); `message` is the
    /// user-facing `YLe` hint (which embeds the code and the remediation).
    #[error("{message}")]
    TlsCert {
        /// The matched TLS error code (a member of the `bBp` set), e.g.
        /// `CERT_HAS_EXPIRED`.
        code: String,
        /// The user-facing SSL fix hint (`YLe`) — includes the code and the
        /// `NODE_EXTRA_CA_CERTS` / `/doctor` remediation.
        message: String,
    },
    /// A stream failed after semantic events had been yielded.
    #[error("stream interrupted: {message}")]
    StreamInterrupted {
        /// Stream interruption detail safe for caller-facing diagnostics.
        message: String,
    },
    /// Model-generated tool arguments were not valid JSON. Never a transport failure.
    #[error("malformed tool input for {tool_name} on block {block_index} ({input_bytes} bytes): {reason}")]
    MalformedToolInput {
        /// Name of the tool whose arguments failed parsing.
        tool_name: String,
        /// Stream content block index.
        block_index: u32,
        /// Parser diagnosis without the generated input.
        reason: String,
        /// Byte length of the malformed arguments.
        input_bytes: usize,
        /// Whether another tool invocation started in this response, or the
        /// stream ended before its absence could be established.
        has_other_tool_calls: bool,
    },
    /// Pricing was required but unavailable.
    #[error("cost unavailable: {message}")]
    CostUnavailable {
        /// Pricing lookup failure message.
        message: String,
    },
    /// Request used a capability the route/model does not support.
    #[error("unsupported capability: {capability}")]
    UnsupportedCapability {
        /// Unsupported capability name.
        capability: String,
    },
    /// Vision delegation could not be performed for a non-vision route.
    #[error("media delegation unavailable: {message}")]
    MediaDelegationUnavailable {
        /// Caller-facing explanation of why delegation is unavailable.
        message: String,
    },
    /// Vision delegation spent one or more provider calls before a later batch
    /// failed. The partial accounting is retained for cost reporting, while the
    /// incomplete analysis is never persisted.
    #[error("media delegation failed: {message}")]
    MediaDelegationPartial {
        /// Caller-facing explanation of the batch failure.
        message: String,
        /// Usage and call accounting from completed/started batches.
        accounting: MediaDelegationAccounting,
    },
}

/// Cost/accounting counters carried across a failed multi-batch delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaDelegationAccounting {
    /// Input tokens from successful provider responses.
    pub input_tokens: u64,
    /// Output tokens from successful provider responses.
    pub output_tokens: u64,
    /// Standard prompt-cache writes.
    pub cache_write: u64,
    /// Prompt-cache reads.
    pub cache_read: u64,
    /// Reasoning output tokens.
    pub reasoning_output: u64,
    /// One-hour prompt-cache writes.
    pub cache_write_1h: u64,
    /// Elapsed wall-clock duration in milliseconds.
    pub elapsed_ms: u64,
    /// Retries reported by completed provider calls.
    pub retry_count: u32,
    /// Provider batches that were started.
    pub api_calls: u32,
}

impl MediaDelegationAccounting {
    /// Build accounting counters from side-query token buckets.
    #[must_use]
    pub fn from_counts(
        input_tokens: u64,
        output_tokens: u64,
        cache_write: u64,
        cache_read: u64,
        reasoning_output: u64,
        cache_write_1h: u64,
        elapsed: Duration,
        retry_count: u32,
        api_calls: u32,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_write,
            cache_read,
            reasoning_output,
            cache_write_1h,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            retry_count,
            api_calls,
        }
    }

    /// Return the elapsed wall-clock duration.
    #[must_use]
    pub fn elapsed(self) -> Duration {
        Duration::from_millis(self.elapsed_ms)
    }
}

impl LlmError {
    /// Build a terminal [`LlmError::TlsCert`] for the given TLS error `code`,
    /// rendering the `YLe` fix hint into `message`.
    ///
    /// Parity: claude-code 2.1.201 — a cause-chain `code` in the `bBp` set
    /// yields `isSSLError`, which short-circuits the retry loop and surfaces the
    /// `YLe` hint (see [`crate::ssl::ssl_hint`]).
    #[must_use]
    pub fn tls_cert(code: impl Into<String>) -> Self {
        let code = code.into();
        let message = crate::ssl::ssl_hint(&code);
        LlmError::TlsCert { code, message }
    }

    /// The matched TLS error code when this is a [`LlmError::TlsCert`], else
    /// `None`. Mirrors reading `JF(e).code` on an `isSSLError` error.
    #[must_use]
    pub fn ssl_code(&self) -> Option<&str> {
        match self {
            LlmError::TlsCert { code, .. } => Some(code.as_str()),
            _ => None,
        }
    }

    /// Recover the HTTP status this error was decoded from, when it is
    /// knowable — the inverse of `providers::api_error_message`.
    ///
    /// claude-code does not keep status in a side field either; the SDK's
    /// `makeMessage` prefixes it onto the message (`${status} ${body}`) and
    /// downstream code parses it back off — `replace(/^429\s+/,"")` @230600821,
    /// `replace(/^400\s+/,"")` @230602614. This is that parse, generalised.
    ///
    /// Returns `None` rather than guessing: a wrong status written to the
    /// transcript's `apiErrorStatus` is worse than an absent one, which is the
    /// same trade the oracle makes when the error is not an `APIError` with a
    /// numeric status.
    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        self.provider_message().and_then(api_error_status)
    }

    /// The PROVIDER's own error text, for variants that preserved it.
    ///
    /// `Display` is not a substitute: several variants render a fixed string
    /// (`"permission denied"`), which is precisely why the auth branches that
    /// gate on the server's wording were unreachable before these variants
    /// carried a message. Returns `None` when the variant has no provider text.
    #[must_use]
    pub fn provider_message(&self) -> Option<&str> {
        match self {
            LlmError::Authentication { message }
            | LlmError::PermissionDenied { message }
            | LlmError::InvalidRequest { message }
            | LlmError::Transport { message }
            | LlmError::TransportTimeout { message }
            | LlmError::StreamInterrupted { message }
            | LlmError::CostUnavailable { message }
            | LlmError::MediaDelegationUnavailable { message }
            | LlmError::MediaDelegationPartial { message, .. } => Some(message),
            _ => None,
        }
    }
}

/// Oracle `sir(e)` @230583592 — the user-facing text for an error.
///
/// This is NOT `Display`. `Display` is the taxonomy's own wording
/// ("provider internal error"); `sir` is what claude-code actually shows, and
/// it has a live consumer here: `ApiService::report_retry` feeds it to the
/// `ApiRetry` event that the TUI renders as the retry banner. That call site
/// was passing `Display`.
///
/// Ported arms:
/// - TLS/SSL codes → the seven `Unable to connect to API: …` forms, default
///   `Unable to connect to API: SSL error ({code})`
/// - `"Connection error."` → `Unable to connect to API…`
/// - empty provider response → `API error (status …)`; an empty local
///   authentication error instead explains that no API key is configured
/// - a message embedding a JSON body → `FOu`: `body.error.message`, else
///   `body.message`, re-prefixed with the status
/// - otherwise the message unchanged
///
/// - timeout → the `ETIMEDOUT` line, via [`LlmError::TransportTimeout`]
///
/// - stream suspend → "Connection interrupted by system sleep", via
///   [`crate::model::stream_watchdog::is_stream_suspended`]
///
/// ⚠️ `BedrockUnexpectedContentType` is NOT special-cased, and does not need to
/// be: the oracle returns the cause message verbatim for it, which is what the
/// default arm here already produces. The only divergence would be a Bedrock
/// content-type message that itself embeds JSON, where the oracle skips the
/// unwrap — and no Bedrock content-type validation exists here to branch on.
#[must_use]
pub fn error_display_text(error: &LlmError) -> String {
    if crate::model::stream_watchdog::is_stream_no_response(error) {
        return "No response from API".to_string();
    }
    if let LlmError::TlsCert { code, .. } = error {
        return match code.as_str() {
            "UNABLE_TO_VERIFY_LEAF_SIGNATURE"
            | "UNABLE_TO_GET_ISSUER_CERT"
            | "UNABLE_TO_GET_ISSUER_CERT_LOCALLY" => "Unable to connect to API: SSL certificate \
                 verification failed. Check your proxy or corporate SSL certificates"
                .to_string(),
            "CERT_HAS_EXPIRED" => {
                "Unable to connect to API: SSL certificate has expired".to_string()
            }
            "CERT_REVOKED" => {
                "Unable to connect to API: SSL certificate has been revoked".to_string()
            }
            "DEPTH_ZERO_SELF_SIGNED_CERT" | "SELF_SIGNED_CERT_IN_CHAIN" => {
                "Unable to connect to API: Self-signed certificate detected. Check your proxy or \
                 corporate SSL certificates"
                    .to_string()
            }
            "ERR_TLS_CERT_ALTNAME_INVALID" | "HOSTNAME_MISMATCH" => {
                "Unable to connect to API: SSL certificate hostname mismatch".to_string()
            }
            "CERT_NOT_YET_VALID" => {
                "Unable to connect to API: SSL certificate is not yet valid".to_string()
            }
            other => format!("Unable to connect to API: SSL error ({other})"),
        };
    }

    let Some(message) = error.provider_message() else {
        // No provider text at all — the taxonomy's own wording is all there is.
        return error.to_string();
    };

    if crate::model::stream_watchdog::is_stream_suspended(error) {
        return "Connection lost while your computer was asleep".to_string();
    }
    if matches!(error, LlmError::TransportTimeout { .. }) {
        return "Request timed out. Check your internet connection and proxy settings".to_string();
    }
    if message == "Connection error." {
        return "Unable to connect to API. Check your internet connection".to_string();
    }
    if message.is_empty() {
        if matches!(error, LlmError::Authentication { .. }) {
            return "No API key is configured for the selected provider".to_string();
        }
        return "API error (status unknown)".to_string();
    }
    api_error_detail(message)
}

/// Oracle `FOu(e)` + `sir`'s `includes('{"')` arm.
///
/// `api_error_message` stringifies the WHOLE body into the message, so the
/// common error reads `403 {"type":"error","error":{"message":"…"}}`. Without
/// this the raw JSON is what a user sees.
#[must_use]
pub fn api_error_detail(message: &str) -> String {
    if !message.contains("{\"") {
        return message.to_string();
    }
    let (status, body) = match message.split_once(' ') {
        Some((head, rest)) if head.len() == 3 && head.bytes().all(|b| b.is_ascii_digit()) => {
            (Some(head), rest)
        }
        _ => (None, message),
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
        return message.to_string();
    };
    let extracted = parsed
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| parsed.get("message").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match (status, extracted) {
        (Some(status), Some(text)) => format!("{status} {text}"),
        (None, Some(text)) => text.to_string(),
        (_, None) => message.to_string(),
    }
}

/// Parse the leading `${status} ` that `api_error_message` writes.
///
/// Deliberately strict, because [`LlmError::InvalidRequest`] has ~200
/// construction sites that never went through a provider decoder and whose
/// messages are arbitrary prose. The shape must be exactly what `makeMessage`
/// emits:
///
/// * three ASCII digits,
/// * in `100..=599` — a real HTTP status, so `"999 bottles"` is prose,
/// * followed by a single space with a non-empty remainder.
///
/// Residual ambiguity is accepted and bounded: prose beginning `"404 "` parses
/// as a status. In practice a decoded provider error's remainder is the
/// stringified body (`{"type":"error",…}`), and the only consumer is an
/// `apiErrorStatus` field that previously held a hardcoded guess — so a rare
/// false positive replaces a systematic one.
#[must_use]
pub fn api_error_status(message: &str) -> Option<u16> {
    let (head, rest) = message.split_at(message.char_indices().nth(3).map_or(0, |(i, _)| i));
    if head.len() != 3 || !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = rest.strip_prefix(' ')?;
    if rest.is_empty() {
        return None;
    }
    let status: u16 = head.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

#[cfg(test)]
mod api_error_status_tests {
    use super::error_display_text;

    /// `sir` is NOT `Display`: it is what claude-code shows, and the retry
    /// banner consumes it. Every string byte-verified against 2.1.220 with a
    /// control that must not match.
    #[test]
    fn sir_renders_the_ssl_family_and_unwraps_json_bodies() {
        let ssl = |code: &str| error_display_text(&LlmError::tls_cert(code));
        assert_eq!(
            ssl("CERT_HAS_EXPIRED"),
            "Unable to connect to API: SSL certificate has expired"
        );
        assert_eq!(
            ssl("CERT_REVOKED"),
            "Unable to connect to API: SSL certificate has been revoked"
        );
        assert_eq!(
            ssl("CERT_NOT_YET_VALID"),
            "Unable to connect to API: SSL certificate is not yet valid"
        );
        assert_eq!(
            ssl("HOSTNAME_MISMATCH"),
            "Unable to connect to API: SSL certificate hostname mismatch"
        );
        assert_eq!(
            ssl("SELF_SIGNED_CERT_IN_CHAIN"),
            "Unable to connect to API: Self-signed certificate detected. Check your proxy or \
             corporate SSL certificates"
        );
        assert_eq!(
            ssl("UNABLE_TO_GET_ISSUER_CERT"),
            "Unable to connect to API: SSL certificate verification failed. Check your proxy or \
             corporate SSL certificates"
        );
        // Unknown code keeps the oracle's parameterised default.
        assert_eq!(ssl("WAT"), "Unable to connect to API: SSL error (WAT)");

        // The suspend arm: the watchdog fired but the WALL clock ran far past
        // the monotonic timeout, which only happens if the machine slept.
        assert_eq!(
            error_display_text(&crate::model::stream_watchdog::watchdog_abort_error(
                std::time::Duration::from_secs(300),
                std::time::Duration::from_secs(900),
            )),
            "Connection lost while your computer was asleep"
        );

        assert_eq!(
            error_display_text(&crate::model::stream_watchdog::first_byte_timeout_error()),
            "No response from API"
        );
        // No drift → an ordinary idle timeout, NOT a suspend.
        assert!(!crate::model::stream_watchdog::is_stream_suspended(
            &crate::model::stream_watchdog::watchdog_abort_error(
                std::time::Duration::from_secs(300),
                std::time::Duration::from_secs(300),
            )
        ));

        // The ETIMEDOUT arm — reachable because `HttpError::Timeout` is typed
        // and `map_http_error` no longer collapses it into `Transport`.
        assert_eq!(
            error_display_text(&LlmError::TransportTimeout {
                message: "request timed out after 30s".to_string()
            }),
            "Request timed out. Check your internet connection and proxy settings"
        );

        // The connection-error arm.
        assert_eq!(
            error_display_text(&LlmError::Transport {
                message: "Connection error.".to_string()
            }),
            "Unable to connect to API. Check your internet connection"
        );

        // The JSON arm — the common shape `api_error_message` produces.
        assert_eq!(
            error_display_text(&LlmError::PermissionDenied {
                message: r#"403 {"type":"error","error":{"message":"revoked"}}"#.to_string()
            }),
            "403 revoked"
        );

        // A variant with no provider text falls back to the taxonomy wording.
        assert_eq!(
            error_display_text(&LlmError::QuotaExceeded),
            "quota exceeded"
        );

        // A locally missing credential is raised before any HTTP request. It
        // must not masquerade as a provider response with an unknown status.
        assert_eq!(
            error_display_text(&LlmError::Authentication {
                message: String::new()
            }),
            "No API key is configured for the selected provider"
        );
    }

    use super::{api_error_status, LlmError};

    #[test]
    fn parses_the_shape_make_message_emits() {
        assert_eq!(
            api_error_status(r#"400 {"type":"error","error":{"message":"bad"}}"#),
            Some(400)
        );
        assert_eq!(api_error_status("429 slow down"), Some(429));
        assert_eq!(api_error_status("503 status code (no body)"), Some(503));
    }

    #[test]
    fn rejects_everything_that_is_not_that_shape() {
        // No space — `makeMessage` always emits exactly one.
        assert_eq!(api_error_status("400bad"), None);
        // Nothing after the space.
        assert_eq!(api_error_status("400 "), None);
        // Not a real HTTP status.
        assert_eq!(api_error_status("999 bottles"), None);
        assert_eq!(api_error_status("099 leading zero"), None);
        assert_eq!(api_error_status("600 too high"), None);
        // Not three digits.
        assert_eq!(api_error_status("40 x"), None);
        assert_eq!(api_error_status("4000 x"), None);
        // Ordinary prose, which is what most InvalidRequest sites carry.
        assert_eq!(api_error_status("invalid model name"), None);
        assert_eq!(api_error_status(""), None);
        // Multi-byte lead must not panic on a non-char-boundary split.
        assert_eq!(api_error_status("\u{4e2d}\u{6587} x"), None);
    }

    #[test]
    fn only_message_bearing_variants_expose_a_status() {
        assert_eq!(
            LlmError::InvalidRequest {
                message: "422 {}".to_string()
            }
            .http_status(),
            Some(422)
        );
        // A variant with no message can never carry a prefix.
        assert_eq!(
            LlmError::Authentication {
                message: String::new()
            }
            .http_status(),
            None
        );
        assert_eq!(LlmError::ProviderInternal.http_status(), None);
    }
}
