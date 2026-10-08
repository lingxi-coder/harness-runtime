//! Tests for `retry.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod retry_test;`.

pub use super::*;

#[cfg(test)]
mod jittered_delay_tests {
    //! Cadence lock tests byte-locked against the v2.1.183 binary's `sle`
    //! retry-delay helper.
    use super::*;

    #[test]
    fn jitter_fraction_is_locked_against_binary() {
        // Binary `sle`: o = r + Math.random() * 0.25 * r.
        assert!((JITTER_FRACTION - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn base_and_cap_constants_match_binary() {
        // Binary: Cbm = 500 (base), sle default cap param n = 32000.
        assert_eq!(BASE_DELAY_MS, 500);
        assert_eq!(MAX_BACKOFF_MS, 32_000);
    }

    #[test]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "ms-scale comparison; the values fit u64 trivially"
    )]
    fn additive_jitter_stays_within_base_to_125_percent() {
        // Binary `sle`: o = r + rand(0, 0.25) * r → uniform over [r, 1.25*r).
        let base = 1_000u64;
        let lo = base; // jitter is additive and non-negative → never below base
        let hi = ((base as f64) * 1.25) as u64;
        for _ in 0..2_000 {
            let d = jittered_delay(base);
            let ms = d.as_millis() as u64;
            assert!(
                ms >= lo && ms < hi,
                "delay {ms}ms outside [{lo}, {hi}) for base={base}",
            );
        }
    }

    /// Binary `sle` base ladder: `min(500 * 2^(attempt-1), 32000)`, 1-indexed.
    /// LingXi's `attempt` is 0-indexed (`A - 1`), so `base_delay_ms(attempt)`
    /// must equal `min(500 * 2^attempt, 32000)`.  Verifies attempts 1..10 of
    /// the binary (i.e. LingXi indices 0..9) plus the cap saturation.
    #[test]
    fn base_delay_ladder_matches_binary_sle() {
        // (lingxi_attempt_index, expected_pre_jitter_base_ms)
        // binary attempt A = index + 1; base = min(500 * 2^(A-1), 32000).
        let expected = [
            (0u32, 500u64), // A=1: 500 * 2^0  = 500
            (1, 1_000),     // A=2: 500 * 2^1  = 1000
            (2, 2_000),     // A=3: 500 * 2^2  = 2000
            (3, 4_000),     // A=4: 500 * 2^3  = 4000
            (4, 8_000),     // A=5: 500 * 2^4  = 8000
            (5, 16_000),    // A=6: 500 * 2^5  = 16000
            (6, 32_000),    // A=7: 500 * 2^6  = 32000 (== cap)
            (7, 32_000),    // A=8: 500 * 2^7  = 64000 → capped at 32000
            (8, 32_000),    // A=9: capped
            (9, 32_000),    // A=10: capped
        ];
        for (attempt, want) in expected {
            assert_eq!(
                base_delay_ms(attempt),
                want,
                "base_delay_ms({attempt}) (binary A={}) should be {want}ms",
                attempt + 1
            );
        }
        // Extreme attempt must not overflow — clamps to the cap.
        assert_eq!(base_delay_ms(200), MAX_BACKOFF_MS);
    }

    /// claude-code `withRetry.ts:52` — `const DEFAULT_MAX_RETRIES = 10`.
    #[test]
    fn default_retry_budget_matches_claude_code() {
        assert_eq!(DEFAULT_MAX_RETRIES, 10);
    }

    /// claude-code `withRetry.ts:789-796` — `LINGXI_MAX_RETRIES` overrides
    /// the default when present and parseable; falls back to 10 otherwise.
    #[test]
    fn claude_code_max_retries_env_overrides() {
        assert_eq!(max_retries_from_env_value(Some("5")), 5);
        // Absent → default.
        assert_eq!(max_retries_from_env_value(None), DEFAULT_MAX_RETRIES);
        // Unparseable → default (mirrors parseInt fallback semantics here).
        assert_eq!(
            max_retries_from_env_value(Some("notanint")),
            DEFAULT_MAX_RETRIES
        );
    }

    #[test]
    fn max_529_retries_is_byte_locked() {
        assert_eq!(MAX_529_RETRIES, 3);
        assert_eq!(RetryControl::default().max_529_retries, 3);
    }
}

#[cfg(test)]
mod next_step_tests {
    use super::*;
    use crate::{LlmError, RetryDecision, RetryPolicy};
    use std::time::Duration;

    /// `state.attempt` value at which the default budget is exhausted (==
    /// [`DEFAULT_MAX_RETRIES`]). Same `u32` type as the `attempt` field.
    const EXHAUSTED_ATTEMPT: u32 = DEFAULT_MAX_RETRIES;

    fn ctl_default() -> RetryControl {
        RetryControl::default()
    }

    /// REGRESSION: a rate-limited turn sat on "Thinking…" for minutes and then
    /// failed anyway.
    ///
    /// When the server names a wait longer than the whole remaining ladder, the
    /// loop gives up before the limit clears — so every sleep in between buys
    /// nothing and is spent to reach the same failure.
    #[test]
    fn a_server_wait_longer_than_the_ladder_fails_fast() {
        let ctl = ctl_default();
        let window = remaining_backoff_window(&RetryState::default(), &ctl, None);
        assert!(
            window > Duration::ZERO,
            "the default ladder must have a window"
        );

        let mut state = RetryState::default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(window + Duration::from_secs(1)),
                scope: None,
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "a wait past the window cannot be retried into success"
        );
        assert_eq!(state.attempt, 0, "no budget consumed");
    }

    /// The other half, and the reason this is not "never retry a 429": burst
    /// throttling names a short wait and clears well inside the ladder. Failing
    /// those fast would turn a self-healing pause into a user-visible error.
    #[test]
    fn a_short_server_wait_still_retries() {
        let ctl = ctl_default();
        let mut state = RetryState::default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
                scope: None,
            },
            0,
        );
        assert!(
            matches!(step, DriveStep::RetryAfter(_)),
            "a 2s wait is what the ladder is for"
        );
        assert_eq!(state.attempt, 1);
    }

    /// A 429 with NO server hint keeps the ladder too — absent is not a signal
    /// that the wait is long, and burst throttling frequently sends none.
    #[test]
    fn a_rate_limit_with_no_hint_keeps_the_ladder() {
        let ctl = ctl_default();
        let mut state = RetryState::default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            0,
        );
        assert!(matches!(step, DriveStep::RetryAfter(_)));
    }

    /// The window shrinks as the budget is spent, so the same server wait can be
    /// retryable early and terminal later. Pins that it is measured from the
    /// CURRENT attempt, not from a fixed constant.
    #[test]
    fn the_window_is_measured_from_the_current_attempt() {
        let ctl = ctl_default();
        let fresh = remaining_backoff_window(&RetryState::default(), &ctl, None);
        let spent = remaining_backoff_window(
            &RetryState {
                attempt: ctl.max_retries - 1,
                ..RetryState::default()
            },
            &ctl,
            None,
        );
        assert!(
            spent < fresh,
            "a nearly-exhausted budget has less window left"
        );
    }

    fn ctl_with_fallback() -> RetryControl {
        RetryControl {
            fallback_model: Some("claude-sonnet-4-6".into()),
            primary_model: "claude-opus-4-6".into(),
            allow_fallback: true,
            is_external: true,
            is_sandbox: false,
            ..RetryControl::default()
        }
    }

    // --- Table point 1: Overloaded ---

    #[test]
    fn overloaded_below_threshold_retries_with_jitter() {
        let mut state = RetryState::default();
        let ctl = ctl_with_fallback();
        // First two overloads: counter < MAX_529_RETRIES (3), budget not exhausted.
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert!(
            matches!(step, DriveStep::RetryAfter(_)),
            "first overload should retry, got {step:?}"
        );
        assert_eq!(state.consecutive_overloaded, 1);
        assert_eq!(state.attempt, 1);

        let step2 = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert!(
            matches!(step2, DriveStep::RetryAfter(_)),
            "second overload should retry, got {step2:?}"
        );
        assert_eq!(state.consecutive_overloaded, 2);
        assert_eq!(state.attempt, 2);
    }

    #[test]
    fn overloaded_at_max_529_retries_with_fallback_triggers_fallback() {
        let mut state = RetryState::default();
        let ctl = ctl_with_fallback();
        // Drive 3 overloads: on the 3rd, consecutive >= MAX_529_RETRIES.
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        ); // consecutive=1
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        ); // consecutive=2
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        ); // consecutive=3
        assert_eq!(
            step,
            DriveStep::Fallback {
                fallback_model: "claude-sonnet-4-6".to_string()
            },
            "third overload should trigger fallback"
        );
    }

    #[test]
    fn overloaded_consecutive_counter_resets_on_non_overloaded() {
        let mut state = RetryState::default();
        let ctl = ctl_with_fallback();
        // Two overloads then a transport error resets the counter.
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert_eq!(state.consecutive_overloaded, 2);

        next_step(
            &mut state,
            &ctl,
            &LlmError::Transport {
                message: "net error".into(),
            },
            0,
        );
        assert_eq!(
            state.consecutive_overloaded, 0,
            "consecutive counter must reset on non-overloaded error"
        );
    }

    #[test]
    fn overloaded_budget_exhausted_is_terminal() {
        let mut state = RetryState {
            attempt: EXHAUSTED_ATTEMPT,
            consecutive_overloaded: 0,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "budget exhausted overloaded must be terminal"
        );
    }

    #[test]
    fn overloaded_without_fallback_configured_retries_then_terminal() {
        // allow_fallback=true but no fallback model → no Fallback, just budget retry.
        let mut state = RetryState::default();
        let ctl = RetryControl {
            allow_fallback: true,
            fallback_model: None,
            primary_model: "claude-opus-4-6".into(),
            ..RetryControl::default()
        };
        // Internal (is_external=false) + no fallback model: the consecutive-529
        // gate never terminates early, so the loop is budget-driven. Drive the
        // full default budget (10 retries) then assert the 11th call is Terminal.
        for _ in 0..DEFAULT_MAX_RETRIES {
            let step = next_step(
                &mut state,
                &ctl,
                &LlmError::Overloaded { repeated: false },
                0,
            );
            assert!(
                matches!(step, DriveStep::RetryAfter(_)),
                "within budget overloaded should RetryAfter, got {step:?}"
            );
        }
        // Budget exhausted: the next call is terminal.
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "overloaded without fallback model should be terminal after budget"
        );
    }

    // --- External no-fallback terminal branch (api-client withRetry.ts:354-359) ---

    /// api-client gate: `allow_fallback && consecutive_529 >= max_529_retries
    ///   && fallback_model.is_none() && is_external && !is_sandbox`
    /// → `RepeatedOverloaded` immediately (does not wait for budget exhaustion).
    ///
    /// The `RepeatedOverloaded` step is distinct from `Terminal` so the adapter
    /// can produce the byte-locked `"Repeated 529 Overloaded errors"` copy
    /// (`errors.ts:166`, TS `withRetry.ts:359-362`).
    #[test]
    fn overloaded_external_without_fallback_terminates_at_threshold() {
        let mut state = RetryState::default();
        let ctl = RetryControl {
            allow_fallback: true,
            fallback_model: None,
            primary_model: "claude-opus-4-6".into(),
            is_external: true,
            is_sandbox: false,
            max_529_retries: MAX_529_RETRIES,
            max_retries: DEFAULT_MAX_RETRIES,
            watchdog: false,
        };
        // Drive consecutive_overloaded up to max_529_retries.
        // Attempts 1 and 2: below threshold, should still retry.
        let s1 = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert!(
            matches!(s1, DriveStep::RetryAfter(_)),
            "attempt 1 should be RetryAfter, got {s1:?}"
        );
        let s2 = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert!(
            matches!(s2, DriveStep::RetryAfter(_)),
            "attempt 2 should be RetryAfter, got {s2:?}"
        );
        // Attempt 3: consecutive_overloaded reaches MAX_529_RETRIES (3).
        // External + no-sandbox + no-fallback → RepeatedOverloaded (before budget exhaustion).
        assert_eq!(
            state.attempt, 2,
            "should have used 2 budget slots (not budget exhausted)"
        );
        let s3 = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        assert_eq!(
            s3,
            DriveStep::RepeatedOverloaded,
            "external no-fallback at threshold must be RepeatedOverloaded, got {s3:?}"
        );
    }

    #[test]
    fn overloaded_sandbox_external_keeps_retrying() {
        // is_sandbox=true disables the early terminal, even if external + no fallback.
        let mut state = RetryState::default();
        let ctl = RetryControl {
            allow_fallback: true,
            fallback_model: None,
            primary_model: "claude-opus-4-6".into(),
            is_external: true,
            is_sandbox: true,
            max_529_retries: MAX_529_RETRIES,
            max_retries: DEFAULT_MAX_RETRIES,
            watchdog: false,
        };
        // Drive through the threshold — sandbox must NOT terminate early.
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        let s3 = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        // consecutive_overloaded == 3 == MAX_529_RETRIES, but is_sandbox → keep retrying.
        assert!(
            matches!(s3, DriveStep::RetryAfter(_)),
            "sandbox should keep retrying past threshold, got {s3:?}"
        );
    }

    #[test]
    fn overloaded_internal_without_fallback_keeps_retrying() {
        // is_external=false → the external terminal branch never fires.
        let mut state = RetryState::default();
        let ctl = RetryControl {
            allow_fallback: true,
            fallback_model: None,
            primary_model: "claude-opus-4-6".into(),
            is_external: false,
            is_sandbox: false,
            max_529_retries: MAX_529_RETRIES,
            max_retries: DEFAULT_MAX_RETRIES,
            watchdog: false,
        };
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        let s3 = next_step(
            &mut state,
            &ctl,
            &LlmError::Overloaded { repeated: false },
            0,
        );
        // consecutive_overloaded == 3, but is_external=false → keep retrying (budget-driven).
        assert!(
            matches!(s3, DriveStep::RetryAfter(_)),
            "internal (non-external) should keep retrying past threshold, got {s3:?}"
        );
    }

    // --- Table point 2: RateLimited + subscriber gate (withRetry.ts:767) ---

    /// withRetry.ts:767: subscriber non-enterprise 429 → terminal (no retry).
    #[test]
    fn non_first_party_rate_limit_fails_fast_first_party_still_retries() {
        // A third-party provider's 429 (e.g. OpenRouter free-tier quota) is
        // TERMINAL — surfaced immediately, no budget consumed — because it does
        // not clear within the backoff window. First-party Anthropic keeps the
        // parity 429-retry.
        let mut third_party = RetryState {
            rate_limit_terminal: true,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        let step = next_step(
            &mut third_party,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
                scope: None,
            },
            0,
        );
        assert_eq!(step, DriveStep::Terminal, "third-party 429 fails fast");
        assert_eq!(third_party.attempt, 0, "no budget consumed on fail-fast");

        // Control: first-party (default `rate_limit_terminal: false`) still retries.
        let mut first_party = RetryState::default();
        let step2 = next_step(
            &mut first_party,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(Duration::from_secs(1)),
                scope: None,
            },
            0,
        );
        assert!(
            matches!(step2, DriveStep::RetryAfter(_)),
            "first-party 429 still retries (parity)"
        );
    }

    #[test]
    fn subscriber_429_is_terminal() {
        let mut state = RetryState {
            is_subscriber: true,
            is_enterprise: false,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(Duration::from_secs(5)),
                scope: None,
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "subscriber non-enterprise 429 must be terminal (withRetry.ts:767)"
        );
        // No budget should be consumed (terminal path).
        assert_eq!(
            state.attempt, 0,
            "terminal 429 must not consume a budget slot"
        );
    }

    /// withRetry.ts:767: enterprise subscriber 429 → retries (enterprise re-enables).
    #[test]
    fn enterprise_subscriber_429_retries() {
        let mut state = RetryState {
            is_subscriber: true,
            is_enterprise: true,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        let server_delay = Duration::from_secs(10);
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(server_delay),
                scope: None,
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::RetryAfter(server_delay),
            "enterprise subscriber 429 must retry (withRetry.ts:767)"
        );
        assert_eq!(state.attempt, 1);
    }

    /// withRetry.ts:767: API-key user (non-subscriber) 429 → retries.
    #[test]
    fn api_key_user_429_retries() {
        let mut state = RetryState {
            is_subscriber: false,
            is_enterprise: false,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            0,
        );
        assert!(
            matches!(step, DriveStep::RetryAfter(_)),
            "API-key (non-subscriber) 429 must retry, got {step:?}"
        );
        assert_eq!(state.attempt, 1);
    }

    #[test]
    fn rate_limited_large_server_delay_exceeds_backoff_floor() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        // 30s server delay far exceeds the attempt-0 backoff (~500ms), so the
        // binary `sle` floor max(server, jittered_backoff) returns the server delay.
        let server_delay = Duration::from_secs(30);
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(server_delay),
                scope: None,
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::RetryAfter(server_delay),
            "server retry_after exceeding the backoff floor is used as-is"
        );
        assert_eq!(state.attempt, 1);
    }

    #[test]
    fn rate_limited_small_server_delay_floored_to_backoff() {
        // A tiny server delay must NOT undercut our own exponential backoff
        // (binary `sle`: max(header, jittered_backoff)).
        let mut state = RetryState::default();
        state.attempt = 5; // backoff base = min(500 * 2^5, 32000) = 16_000ms
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: Some(Duration::from_millis(100)),
                scope: None,
            },
            0,
        );
        match step {
            DriveStep::RetryAfter(d) => assert!(
                d >= Duration::from_millis(16_000),
                "100ms server delay must be floored to the ~16s backoff, got {d:?}"
            ),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn rate_limited_no_server_delay_uses_jitter() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            0,
        );
        assert!(
            matches!(step, DriveStep::RetryAfter(_)),
            "rate-limited without server delay should retry with jitter, got {step:?}"
        );
        assert_eq!(state.attempt, 1);
    }

    #[test]
    fn rate_limited_resets_consecutive_overloaded() {
        let mut state = RetryState {
            attempt: 0,
            consecutive_overloaded: 2,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        next_step(
            &mut state,
            &ctl,
            &LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            0,
        );
        assert_eq!(
            state.consecutive_overloaded, 0,
            "rate-limited must reset consecutive counter"
        );
    }

    // --- Table point 3: ProviderInternal / Transport ---

    #[test]
    fn provider_internal_retries_within_budget() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
        assert!(
            matches!(step, DriveStep::RetryAfter(_)),
            "ProviderInternal should retry within budget, got {step:?}"
        );
        assert_eq!(state.attempt, 1);
    }

    #[test]
    fn transport_error_retries_within_budget() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::Transport {
                message: "connection refused".into(),
            },
            0,
        );
        assert!(
            matches!(step, DriveStep::RetryAfter(_)),
            "Transport should retry within budget, got {step:?}"
        );
        assert_eq!(state.attempt, 1);
    }

    #[test]
    fn provider_internal_budget_exhausted_is_terminal() {
        let mut state = RetryState {
            attempt: EXHAUSTED_ATTEMPT,
            consecutive_overloaded: 0,
            ..RetryState::default()
        };
        let ctl = ctl_default();
        let step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
        assert_eq!(
            step,
            DriveStep::Terminal,
            "ProviderInternal with exhausted budget must be Terminal"
        );
    }

    // --- Table point 4: InvalidRequest / overflow ---

    #[test]
    fn invalid_request_overflow_message_adjusts_max_tokens() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let msg = "input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000";
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::InvalidRequest {
                message: msg.to_string(),
            },
            0,
        );
        // adjusted: available = 200000 - 188059 - 1000 = 10941
        assert_eq!(
            step,
            DriveStep::AdjustMaxTokens(10_941),
            "overflow message should yield AdjustMaxTokens"
        );
        // AdjustMaxTokens must NOT consume a budget attempt.
        assert_eq!(
            state.attempt, 0,
            "AdjustMaxTokens must not increment attempt"
        );
    }

    #[test]
    fn openrouter_overflow_message_adjusts_max_tokens() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let msg = "This endpoint's maximum context length is 256000 tokens. However, you requested about 260085 tokens (10691 of text input, 24791 of tool input, 224603 in the output). Please reduce the length of either one.";
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::InvalidRequest {
                message: msg.to_string(),
            },
            0,
        );

        assert_eq!(step, DriveStep::AdjustMaxTokens(219_518));
        assert_eq!(state.attempt, 0);
    }

    #[test]
    fn invalid_request_non_overflow_is_terminal() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::InvalidRequest {
                message: "some other validation error".to_string(),
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "non-overflow InvalidRequest must be Terminal"
        );
    }

    #[test]
    fn thinking_signature_400_strips_once_then_terminals() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let err = LlmError::InvalidRequest {
            message: "Invalid signature in thinking block".into(),
        };
        let step = next_step(&mut state, &ctl, &err, 0);
        assert_eq!(step, DriveStep::StripThinkingSignature);
        assert_eq!(
            state.attempt, 0,
            "StripThinkingSignature must not consume a budget attempt"
        );
        assert!(state.thinking_signature_stripped);

        let again = next_step(&mut state, &ctl, &err, 0);
        assert_eq!(
            again,
            DriveStep::Terminal,
            "a second thinking-signature 400 on the same request is terminal"
        );
    }

    #[test]
    fn deepseek_reasoning_content_400_is_terminal() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::InvalidRequest {
                message:
                    "The reasoning_content in the thinking mode must be passed back to the API."
                        .into(),
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "DeepSeek reasoning round-trip 400 must not strip thinking"
        );
        assert!(!state.thinking_signature_stripped);
    }

    #[test]
    fn invalid_request_overflow_but_no_room_is_terminal() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        // available = 200000 - 199000 - 1000 = 0 < 3000 → adjusted_max_tokens returns None.
        let msg = "input length and `max_tokens` exceed context limit: 199000 + 20000 > 200000";
        let step = next_step(
            &mut state,
            &ctl,
            &LlmError::InvalidRequest {
                message: msg.to_string(),
            },
            0,
        );
        assert_eq!(
            step,
            DriveStep::Terminal,
            "overflow with no room should be Terminal (not AdjustMaxTokens)"
        );
    }

    // --- Table point 5: always-terminal errors ---

    #[test]
    fn terminal_error_classes_are_terminal() {
        let errors = vec![
            LlmError::Authentication {
                message: String::new(),
            },
            LlmError::PermissionDenied {
                message: String::new(),
            },
            LlmError::ContextOverflow { token_gap: 0 },
            LlmError::QuotaExceeded,
            LlmError::ModelUnavailable,
            LlmError::StreamInterrupted {
                message: "eof".into(),
            },
            LlmError::MalformedToolInput {
                tool_name: "StructuredOutput".into(),
                block_index: 0,
                reason: "EOF while parsing an object".into(),
                input_bytes: 1,
                has_other_tool_calls: false,
            },
            LlmError::CostUnavailable {
                message: "no price".into(),
            },
            LlmError::UnsupportedCapability {
                capability: "thinking".into(),
            },
        ];
        for error in &errors {
            let mut state = RetryState::default();
            let ctl = ctl_default();
            let step = next_step(&mut state, &ctl, error, 0);
            assert_eq!(step, DriveStep::Terminal, "{error:?} should be Terminal");
        }
    }

    // --- Table point 6: budget exhaustion for retryable classes ---

    #[test]
    fn budget_exhaustion_makes_all_retryable_classes_terminal() {
        let retryable_errors = vec![
            LlmError::ProviderInternal,
            LlmError::Transport {
                message: "net".into(),
            },
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            // Overloaded without fallback configured:
            LlmError::Overloaded { repeated: false },
        ];
        for error in &retryable_errors {
            let mut state = RetryState {
                attempt: EXHAUSTED_ATTEMPT,
                consecutive_overloaded: 0,
                ..RetryState::default()
            };
            let ctl = ctl_default();
            let step = next_step(&mut state, &ctl, error, 0);
            assert_eq!(
                step,
                DriveStep::Terminal,
                "budget-exhausted {error:?} must be Terminal"
            );
        }
    }

    // --- Loop bound: initial + DEFAULT_MAX_RETRIES retries (claude-code withRetry.ts:189) ---

    /// claude-code `withRetry.ts:189` — `for (attempt = 1; attempt <= maxRetries + 1; attempt++)`
    /// at the default permits up to 11 executions / 10 sleeps. Modeled here as
    /// `next_step` returning `RetryAfter` for the first 10 invocations (each
    /// consuming a budget slot / sleep) and only becoming `Terminal` on the
    /// 11th invocation (the 11th execution would have no further retry).
    #[test]
    fn next_step_terminal_after_eleven_executions_at_default() {
        let mut state = RetryState::default();
        let ctl = ctl_default();
        // 10 retryable failures each yield RetryAfter (10 sleeps after the
        // initial execution = 11 total executions worth of attempts).
        for i in 0..DEFAULT_MAX_RETRIES {
            let step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
            assert!(
                matches!(step, DriveStep::RetryAfter(_)),
                "invocation {i} (attempt before={}) should RetryAfter, got {step:?}",
                state.attempt
            );
        }
        assert_eq!(
            state.attempt, DEFAULT_MAX_RETRIES,
            "after 10 RetryAfter steps the attempt counter equals the budget"
        );
        // 11th invocation: budget exhausted → Terminal.
        let final_step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
        assert_eq!(
            final_step,
            DriveStep::Terminal,
            "the 11th execution attempt must be Terminal at the default budget"
        );
    }

    // --- Table point 8: cross-check with RetryPolicy ---

    /// Every error that `RetryPolicy::classify_error` calls `DoNotRetry` must
    /// yield `DriveStep::Terminal` from `next_step` (except the `AdjustMaxTokens`
    /// special case for `InvalidRequest`). Conversely, every error that
    /// `RetryPolicy` calls `Retry` must yield a non-terminal step when the
    /// budget is fresh.
    ///
    /// This test ensures the two classification layers cannot silently drift.
    #[test]
    fn retry_policy_and_next_step_agree_on_terminal_vs_retryable() {
        let policy = RetryPolicy;

        // Errors that RetryPolicy says DoNotRetry — must all be Terminal from
        // next_step (except InvalidRequest overflow, which is tested separately).
        let do_not_retry = vec![
            LlmError::Authentication {
                message: String::new(),
            },
            LlmError::PermissionDenied {
                message: String::new(),
            },
            LlmError::ContextOverflow { token_gap: 0 },
            LlmError::QuotaExceeded,
            LlmError::ModelUnavailable,
            LlmError::StreamInterrupted {
                message: "x".into(),
            },
            LlmError::CostUnavailable {
                message: "x".into(),
            },
            LlmError::UnsupportedCapability {
                capability: "x".into(),
            },
        ];
        for error in &do_not_retry {
            assert_eq!(
                policy.classify_error(error),
                RetryDecision::DoNotRetry,
                "RetryPolicy expected DoNotRetry for {error:?}"
            );
            let mut state = RetryState::default();
            let ctl = ctl_default();
            let step = next_step(&mut state, &ctl, error, 0);
            assert_eq!(
                step,
                DriveStep::Terminal,
                "next_step must be Terminal for {error:?} (DoNotRetry in RetryPolicy)"
            );
        }

        // Errors that RetryPolicy says Retry — must all be non-terminal from
        // next_step when budget is fresh.
        let do_retry = vec![
            LlmError::ProviderInternal,
            LlmError::Overloaded { repeated: false },
            LlmError::Transport {
                message: "err".into(),
            },
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
        ];
        for error in &do_retry {
            assert!(
                matches!(policy.classify_error(error), RetryDecision::Retry { .. }),
                "RetryPolicy expected Retry for {error:?}"
            );
            let mut state = RetryState::default();
            let ctl = ctl_default();
            let step = next_step(&mut state, &ctl, error, 0);
            assert_ne!(
                step,
                DriveStep::Terminal,
                "next_step must be non-terminal for {error:?} (Retry in RetryPolicy) with fresh budget"
            );
        }

        // InvalidRequest: RetryPolicy says DoNotRetry; next_step may yield
        // AdjustMaxTokens (overflow) or Terminal (non-overflow). Both are
        // acceptable; the AdjustMaxTokens case is documented as the special case.
        let invalid_non_overflow = LlmError::InvalidRequest {
            message: "bad param".into(),
        };
        assert_eq!(
            policy.classify_error(&invalid_non_overflow),
            RetryDecision::DoNotRetry,
        );
        let mut state = RetryState::default();
        let step = next_step(&mut state, &ctl_default(), &invalid_non_overflow, 0);
        assert_eq!(
            step,
            DriveStep::Terminal,
            "non-overflow InvalidRequest must be Terminal (matches RetryPolicy DoNotRetry)"
        );
    }

    // --- Backoff schedule byte-locked against the v2.1.183 binary `sle` ---

    /// `next_step` must produce the binary `sle` schedule for the retryable
    /// classes (ProviderInternal/Transport/Overloaded-no-fallback/RateLimited-
    /// no-header): for each attempt the delay lies in `[base, 1.25 * base)`
    /// where `base = min(500 * 2^attempt, 32000)` (binary 1-indexed
    /// `A = attempt + 1`).  Covers the binary's attempts 1..10.
    #[test]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_lossless,
        reason = "ms-scale comparison; the values fit u64 trivially"
    )]
    fn next_step_delay_schedule_matches_binary_sle() {
        // (lingxi attempt index 0..9, expected pre-jitter base = min(500*2^i, 32000))
        let schedule: [(u32, u64); 10] = [
            (0, 500),
            (1, 1_000),
            (2, 2_000),
            (3, 4_000),
            (4, 8_000),
            (5, 16_000),
            (6, 32_000),
            (7, 32_000),
            (8, 32_000),
            (9, 32_000),
        ];
        for (attempt_idx, base) in schedule {
            let lo = base; // additive non-negative jitter → never below base
            let hi = ((base as f64) * 1.25) as u64;
            // Sample several times to catch stochastic issues.
            for _ in 0..50 {
                let mut state = RetryState {
                    attempt: attempt_idx,
                    consecutive_overloaded: 0,
                    ..RetryState::default()
                };
                let ctl = ctl_default();
                let step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
                let DriveStep::RetryAfter(d) = step else {
                    panic!("expected RetryAfter for attempt={attempt_idx}, got {step:?}");
                };
                #[allow(clippy::cast_possible_truncation)]
                let ms = d.as_millis() as u64;
                assert!(
                    ms >= lo && ms < hi,
                    "attempt={attempt_idx} (binary A={}): delay {ms}ms outside [{lo},{hi}) for base={base}",
                    attempt_idx + 1
                );
            }
        }
    }

    /// Spot-check the exact pre-jitter base ladder one more time through the
    /// `scaled_base_delay_ms(_, None)` path used by `next_step` (no override),
    /// pinning the exponential cap behaviour independent of the random jitter.
    #[test]
    fn scaled_base_delay_default_path_is_exponential_capped() {
        assert_eq!(scaled_base_delay_ms(0, None), 500);
        assert_eq!(scaled_base_delay_ms(1, None), 1_000);
        assert_eq!(scaled_base_delay_ms(2, None), 2_000);
        assert_eq!(scaled_base_delay_ms(5, None), 16_000);
        assert_eq!(scaled_base_delay_ms(6, None), 32_000);
        assert_eq!(scaled_base_delay_ms(20, None), 32_000); // capped, no overflow
                                                            // backoff_ms override sets the first rung but keeps exponential + cap.
        assert_eq!(scaled_base_delay_ms(0, Some(1_000)), 1_000);
        assert_eq!(scaled_base_delay_ms(1, Some(1_000)), 2_000);
        assert_eq!(scaled_base_delay_ms(5, Some(1_000)), 32_000); // 1000*2^5=32000 == cap
        assert_eq!(scaled_base_delay_ms(6, Some(1_000)), 32_000); // 1000*2^6=64000 → capped
    }
}

#[cfg(test)]
mod resolve_retry_control_tests {
    //! Env-matrix tests mirroring api-client's `resolve_retry_control` tests.
    use super::*;
    use crate::LlmError;

    fn env(
        fallback_for_all: Option<&str>,
        user_type: Option<&str>,
        is_sandbox: bool,
    ) -> ResolveRetryEnv {
        ResolveRetryEnv {
            fallback_for_all: fallback_for_all.map(str::to_string),
            user_type: user_type.map(str::to_string),
            is_sandbox_defined: is_sandbox,
            max_retries: None,
            retry_watchdog: false,
        }
    }

    const OPUS_MODEL: &str = "claude-opus-4-6";
    const SONNET_MODEL: &str = "claude-sonnet-4-20250514";

    // --- allow_fallback ---

    /// `FALLBACK_FOR_ALL_PRIMARY_MODELS` non-empty → `allow_fallback = true`
    /// regardless of model or subscriber status.
    #[test]
    fn fallback_for_all_truthy_enables_allow_fallback() {
        let ctl = resolve_retry_control(
            SONNET_MODEL,
            None,
            true, // is_subscriber
            &env(Some("1"), None, false),
        );
        assert!(
            ctl.allow_fallback,
            "non-empty FALLBACK_FOR_ALL_PRIMARY_MODELS must set allow_fallback"
        );
    }

    /// `FALLBACK_FOR_ALL_PRIMARY_MODELS` empty string is falsy (JS semantics).
    #[test]
    fn fallback_for_all_empty_string_is_falsy() {
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env(Some(""), None, false));
        assert!(
            !ctl.allow_fallback,
            "empty FALLBACK_FOR_ALL_PRIMARY_MODELS must be falsy"
        );
    }

    /// Non-subscriber + Opus model → `allow_fallback = true`.
    #[test]
    fn non_subscriber_opus_allows_fallback() {
        let ctl = resolve_retry_control(
            OPUS_MODEL,
            Some("claude-sonnet-4-6".into()),
            false, // not subscriber
            &env(None, None, false),
        );
        assert!(ctl.allow_fallback, "non-subscriber + opus → allow_fallback");
    }

    /// Subscriber + Opus model → `allow_fallback = false` (unless `FALLBACK_FOR_ALL`).
    #[test]
    fn subscriber_opus_no_fallback_unless_env() {
        let ctl = resolve_retry_control(
            OPUS_MODEL,
            Some("claude-sonnet-4-6".into()),
            true, // subscriber
            &env(None, None, false),
        );
        assert!(
            !ctl.allow_fallback,
            "subscriber + opus must NOT set allow_fallback unless FALLBACK_FOR_ALL"
        );
    }

    /// Non-subscriber + non-Opus model → `allow_fallback = false`.
    #[test]
    fn non_subscriber_non_opus_no_fallback() {
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env(None, None, false));
        assert!(
            !ctl.allow_fallback,
            "non-subscriber + non-opus must NOT allow fallback"
        );
    }

    // --- is_external ---

    #[test]
    fn user_type_external_sets_is_external() {
        let ctl = resolve_retry_control(
            SONNET_MODEL,
            None,
            false,
            &env(None, Some("external"), false),
        );
        assert!(ctl.is_external);
    }

    #[test]
    fn user_type_non_external_clears_is_external() {
        let ctl = resolve_retry_control(
            SONNET_MODEL,
            None,
            false,
            &env(None, Some("internal"), false),
        );
        assert!(!ctl.is_external);
    }

    #[test]
    fn user_type_absent_clears_is_external() {
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env(None, None, false));
        assert!(!ctl.is_external);
    }

    // --- is_sandbox ---

    #[test]
    fn is_sandbox_defined_sets_is_sandbox() {
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env(None, None, true));
        assert!(ctl.is_sandbox);
    }

    #[test]
    fn is_sandbox_absent_clears_is_sandbox() {
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env(None, None, false));
        assert!(!ctl.is_sandbox);
    }

    // --- fallback_model is threaded through ---

    #[test]
    fn fallback_model_is_threaded_through() {
        let ctl = resolve_retry_control(
            SONNET_MODEL,
            Some("claude-sonnet-4-6".into()),
            false,
            &env(None, None, false),
        );
        assert_eq!(ctl.fallback_model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(ctl.primary_model, SONNET_MODEL);
    }

    // --- from_process_env smoke test (should not panic) ---

    #[test]
    fn from_process_env_does_not_panic() {
        let _ = ResolveRetryEnv::from_process_env();
    }

    // --- Fix 1: LINGXI_MAX_RETRIES drives next_step via resolve_retry_control ---

    /// `LINGXI_MAX_RETRIES=2` → `ctl.max_retries = 2` → `next_step` returns
    /// `Terminal` after 3 executions (2 sleeps), not 11.
    ///
    /// Mirrors `withRetry.ts:789-796` `getMaxRetries → options.maxRetries ??
    /// LINGXI_MAX_RETRIES ?? 10`.
    #[test]
    fn claude_code_max_retries_env_drives_next_step() {
        // Inject max_retries="2" via ResolveRetryEnv (avoids mutating std::env).
        let env = ResolveRetryEnv {
            max_retries: Some("2".to_string()),
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env);
        assert_eq!(ctl.max_retries, 2, "ctl.max_retries must be 2");

        let mut state = RetryState::default();
        // First 2 calls → RetryAfter (2 sleeps).
        for i in 0..2u32 {
            let step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
            assert!(
                matches!(step, DriveStep::RetryAfter(_)),
                "call {i}: expected RetryAfter, got {step:?}"
            );
        }
        // 3rd call → budget exhausted (attempt=2 >= max_retries=2) → Terminal.
        let final_step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
        assert_eq!(
            final_step,
            DriveStep::Terminal,
            "3rd call must be Terminal (LINGXI_MAX_RETRIES=2)"
        );
    }

    /// Absent or unparseable `LINGXI_MAX_RETRIES` falls back to `DEFAULT_MAX_RETRIES`.
    #[test]
    fn claude_code_max_retries_absent_uses_default() {
        let env = ResolveRetryEnv::default(); // max_retries: None
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env);
        assert_eq!(
            ctl.max_retries, DEFAULT_MAX_RETRIES,
            "absent LINGXI_MAX_RETRIES must default to {DEFAULT_MAX_RETRIES}"
        );
    }

    /// Unparseable value falls back to `DEFAULT_MAX_RETRIES` (mirrors parseInt semantics).
    #[test]
    fn claude_code_max_retries_unparseable_uses_default() {
        let env = ResolveRetryEnv {
            max_retries: Some("notanumber".to_string()),
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control(SONNET_MODEL, None, false, &env);
        assert_eq!(
            ctl.max_retries, DEFAULT_MAX_RETRIES,
            "unparseable LINGXI_MAX_RETRIES must default to {DEFAULT_MAX_RETRIES}"
        );
    }

    // ── Precedence: env > settings > default ──────────────────────────────────

    /// `LINGXI_MAX_RETRIES` env beats `settings_max_retries`.
    #[test]
    fn env_beats_settings_max_retries() {
        let env = ResolveRetryEnv {
            max_retries: Some("2".to_string()), // env says 2
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control_with_settings(SONNET_MODEL, None, false, &env, Some(7));
        assert_eq!(ctl.max_retries, 2, "env(2) must beat settings(7)");
    }

    /// `settings_max_retries` beats `DEFAULT_MAX_RETRIES` when env absent.
    #[test]
    fn settings_beats_default_max_retries() {
        let env = ResolveRetryEnv::default(); // no env var
        let ctl = resolve_retry_control_with_settings(SONNET_MODEL, None, false, &env, Some(7));
        assert_eq!(
            ctl.max_retries, 7,
            "settings(7) must beat default(10) when env absent"
        );
    }

    /// When both env and settings absent, DEFAULT applies.
    #[test]
    fn neither_env_nor_settings_gives_default() {
        let env = ResolveRetryEnv::default();
        let ctl = resolve_retry_control_with_settings(SONNET_MODEL, None, false, &env, None);
        assert_eq!(ctl.max_retries, DEFAULT_MAX_RETRIES);
    }
}

#[cfg(test)]
mod backoff_scaling_tests {
    use super::*;

    /// `scaled_base_delay_ms` with `None` matches `base_delay_ms`.
    #[test]
    fn scaled_none_matches_default() {
        for attempt in 0u32..5 {
            assert_eq!(
                scaled_base_delay_ms(attempt, None),
                base_delay_ms(attempt),
                "scaled(None) must equal base_delay_ms for attempt={attempt}"
            );
        }
    }

    /// `backoff_ms=1000` → exponential ladder `min(1000 * 2^attempt, 32000)`
    /// = `[1000, 2000, 4000, 8000, 16000, 32000, …]` (binary `sle` with base 1000).
    #[test]
    fn backoff_1000_scales_exponentially_with_cap() {
        assert_eq!(scaled_base_delay_ms(0, Some(1000)), 1000);
        assert_eq!(scaled_base_delay_ms(1, Some(1000)), 2000);
        assert_eq!(scaled_base_delay_ms(2, Some(1000)), 4000);
        assert_eq!(scaled_base_delay_ms(3, Some(1000)), 8000);
        assert_eq!(scaled_base_delay_ms(4, Some(1000)), 16000);
        assert_eq!(scaled_base_delay_ms(5, Some(1000)), 32000); // 1000*2^5 == cap
                                                                // Beyond the cap (and at extreme attempts) clamps to MAX_BACKOFF_MS, no overflow.
        assert_eq!(scaled_base_delay_ms(6, Some(1000)), MAX_BACKOFF_MS);
        assert_eq!(scaled_base_delay_ms(10, Some(1000)), MAX_BACKOFF_MS);
    }

    /// `backoff_ms=500` → same as default (ratio = 1).
    #[test]
    fn backoff_500_same_as_default() {
        for attempt in 0u32..5 {
            assert_eq!(
                scaled_base_delay_ms(attempt, Some(500)),
                base_delay_ms(attempt),
                "backoff_ms=500 should reproduce default ladder at attempt={attempt}"
            );
        }
    }

    /// `backoff_ms=250` → exponential ladder `min(250 * 2^attempt, 32000)`
    /// = `[250, 500, 1000, …]` (base halved relative to the default 500).
    #[test]
    fn backoff_250_exponential_from_lower_base() {
        assert_eq!(scaled_base_delay_ms(0, Some(250)), 250);
        assert_eq!(scaled_base_delay_ms(1, Some(250)), 500);
        assert_eq!(scaled_base_delay_ms(2, Some(250)), 1000);
    }

    /// `backoff_ms=0` (unreachable via settings — parse rejects it) floors at
    /// 1ms instead of producing a zero-delay tight retry loop.
    #[test]
    fn backoff_zero_floors_at_one_ms() {
        assert_eq!(scaled_base_delay_ms(0, Some(0)), 1);
        assert_eq!(scaled_base_delay_ms(2, Some(0)), 1);
    }
}

/// SSL/cert fast-fail (parity 2.1.201 `isSSLError` short-circuit).
#[cfg(test)]
mod ssl_fast_fail_tests {
    use super::*;
    use crate::{LlmError, RetryDecision, RetryPolicy};

    /// A TLS/cert error terminates immediately on the FIRST failure, never
    /// consuming the retry budget (no `RetryAfter`) — even with a huge budget.
    #[test]
    fn tls_cert_is_terminal_immediately() {
        let mut state = RetryState::default();
        let ctl = RetryControl {
            max_retries: DEFAULT_MAX_RETRIES,
            ..RetryControl::default()
        };
        let step = next_step(&mut state, &ctl, &LlmError::tls_cert("CERT_HAS_EXPIRED"), 0);
        assert_eq!(
            step,
            DriveStep::Terminal,
            "SSL cert error must be terminal on the first failure, got {step:?}"
        );
        // Budget untouched — no attempt consumed.
        assert_eq!(state.attempt, 0);
    }

    /// `RetryPolicy::classify_error` maps a TLS/cert error to `DoNotRetry`
    /// (contrast: a plain `Transport` error is `Retry`).
    #[test]
    fn classify_error_does_not_retry_ssl() {
        let policy = RetryPolicy;
        assert_eq!(
            policy.classify_error(&LlmError::tls_cert("SELF_SIGNED_CERT_IN_CHAIN")),
            RetryDecision::DoNotRetry
        );
        // Sanity: a non-SSL transport error still retries.
        assert!(matches!(
            policy.classify_error(&LlmError::Transport {
                message: "connection reset".into()
            }),
            RetryDecision::Retry { .. }
        ));
    }

    /// The terminal error's `Display` carries the `YLe` fix hint (so callers
    /// surface it verbatim), and `ssl_code()` exposes the matched code.
    #[test]
    fn tls_cert_surfaces_hint_and_code() {
        let err = LlmError::tls_cert("CERT_HAS_EXPIRED");
        assert_eq!(err.ssl_code(), Some("CERT_HAS_EXPIRED"));
        let shown = err.to_string();
        assert!(
            shown.starts_with("SSL certificate error (CERT_HAS_EXPIRED)."),
            "Display must lead with the SSL hint, got: {shown}"
        );
        assert!(shown.contains("NODE_EXTRA_CA_CERTS"));
        assert!(shown.contains("/doctor"));
    }
}

/// Retry-watchdog (`CLAUDE_CODE_RETRY_WATCHDOG`) — pDs port + capacity exemption
/// (parity 2.1.207 H-CHG-P3B).
#[cfg(test)]
mod retry_watchdog_tests {
    use super::*;
    use crate::LlmError;

    #[test]
    fn watchdog_flag_uses_current_typed_boolean_rules() {
        for value in ["1", "true", "YES", "on", "\u{feff}true\u{feff}"] {
            assert!(retry_watchdog_from_value(Some(value)), "{value:?}");
        }
        for value in [
            None,
            Some("0"),
            Some("false"),
            Some(""),
            Some("\u{0085}true\u{0085}"),
        ] {
            assert!(!retry_watchdog_from_value(value), "{value:?}");
        }
    }

    // ── pDs(): resolve_max_retries matrix ──

    #[test]
    fn pds_default_no_env() {
        // {env unset, watchdog off} → 10 ; {watchdog on} → 300.
        assert_eq!(resolve_max_retries(false, None), DEFAULT_MAX_RETRIES);
        assert_eq!(resolve_max_retries(true, None), WATCHDOG_MAX_RETRIES);
        assert_eq!(WATCHDOG_MAX_RETRIES, 300);
    }

    #[test]
    fn pds_explicit_within_cap_passes_through() {
        assert_eq!(resolve_max_retries(false, Some("5")), 5);
        assert_eq!(resolve_max_retries(true, Some("5")), 5);
        // Exactly at the clamp boundary (15) is not clamped.
        assert_eq!(resolve_max_retries(false, Some("15")), 15);
    }

    #[test]
    fn pds_clamps_over_15_when_watchdog_off() {
        // {MAX_RETRIES=50, watchdog off} → 15 (clamped).
        assert_eq!(resolve_max_retries(false, Some("50")), MAX_RETRIES_CLAMP);
        assert_eq!(MAX_RETRIES_CLAMP, 15);
    }

    #[test]
    fn pds_watchdog_lifts_the_clamp() {
        // {MAX_RETRIES=50, watchdog on} → 50 (uncapped).
        assert_eq!(resolve_max_retries(true, Some("50")), 50);
        assert_eq!(resolve_max_retries(true, Some("100000")), 100_000);
    }

    #[test]
    fn pds_unparseable_falls_to_default() {
        // JS `if(Number.isFinite(t)&&t>=0)` false → `return e?_j_:yj_`.
        assert_eq!(
            resolve_max_retries(false, Some("notanint")),
            DEFAULT_MAX_RETRIES
        );
        assert_eq!(
            resolve_max_retries(true, Some("notanint")),
            WATCHDOG_MAX_RETRIES
        );
        // Empty string is falsy in JS (`if(process.env.X)` false) → default.
        assert_eq!(resolve_max_retries(false, Some("")), DEFAULT_MAX_RETRIES);
    }

    // ── resolve_retry_control threads the watchdog ──

    #[test]
    fn resolve_control_sets_watchdog_and_default_300() {
        let env = ResolveRetryEnv {
            retry_watchdog: true,
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control("claude-sonnet-4-20250514", None, false, &env);
        assert!(ctl.watchdog, "ctl.watchdog must be set from env");
        assert_eq!(
            ctl.max_retries, WATCHDOG_MAX_RETRIES,
            "watchdog default budget is 300"
        );
    }

    #[test]
    fn resolve_control_watchdog_lifts_env_clamp() {
        let env = ResolveRetryEnv {
            retry_watchdog: true,
            max_retries: Some("50".to_string()),
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control("claude-sonnet-4-20250514", None, false, &env);
        assert_eq!(ctl.max_retries, 50, "watchdog lifts the >15 clamp");
    }

    #[test]
    fn resolve_control_clamps_env_without_watchdog() {
        let env = ResolveRetryEnv {
            retry_watchdog: false,
            max_retries: Some("50".to_string()),
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control("claude-sonnet-4-20250514", None, false, &env);
        assert_eq!(
            ctl.max_retries, MAX_RETRIES_CLAMP,
            "no watchdog → clamp to 15"
        );
    }

    // ── next_step: capacity errors are exempt from budget under watchdog ──

    fn watchdog_ctl() -> RetryControl {
        RetryControl {
            max_retries: 3, // tiny budget to prove the exemption
            watchdog: true,
            ..RetryControl::default()
        }
    }

    #[test]
    fn watchdog_overloaded_never_exhausts_budget() {
        let ctl = watchdog_ctl();
        let mut state = RetryState::default();
        // Far past max_retries (3): every 529 still retries under the watchdog.
        for i in 0..40u32 {
            let step = next_step(
                &mut state,
                &ctl,
                &LlmError::Overloaded { repeated: false },
                0,
            );
            assert!(
                matches!(step, DriveStep::RetryAfter(_)),
                "iter {i}: 529 under watchdog must retry, got {step:?}"
            );
        }
    }

    #[test]
    fn watchdog_rate_limited_never_exhausts_budget() {
        let ctl = watchdog_ctl();
        let mut state = RetryState::default();
        for i in 0..40u32 {
            let step = next_step(
                &mut state,
                &ctl,
                &LlmError::RateLimited {
                    retry_after: None,
                    scope: None,
                },
                0,
            );
            assert!(
                matches!(step, DriveStep::RetryAfter(_)),
                "iter {i}: 429 under watchdog must retry, got {step:?}"
            );
        }
    }

    #[test]
    fn watchdog_capacity_backoff_and_ordinary_retry_counts_are_independent() {
        let ctl = RetryControl {
            max_retries: 1,
            watchdog: true,
            ..RetryControl::default()
        };
        let mut state = RetryState::default();
        for error in [
            LlmError::Overloaded { repeated: false },
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
        ] {
            assert!(matches!(
                next_step_with_backoff(&mut state, &ctl, &error, 0, Some(10)),
                DriveStep::RetryAfter(_)
            ));
        }
        assert_eq!(state.attempt, 0);
        assert_eq!(state.watchdog_capacity_waits, 2);
        let DriveStep::RetryAfter(delay) =
            next_step_with_backoff(&mut state, &ctl, &LlmError::ProviderInternal, 0, Some(10))
        else {
            panic!("the ordinary retry budget must remain available");
        };
        // Ese's ordinary error ladder uses $t+B: two capacity waits advance
        // the delay to 40ms, but do not spend the one ordinary retry.
        assert!(delay.as_millis() >= 40 && delay.as_millis() < 50);
        assert_eq!(state.attempt, 1);
        assert_eq!(
            next_step_with_backoff(&mut state, &ctl, &LlmError::ProviderInternal, 0, Some(10)),
            DriveStep::Terminal
        );
        assert_eq!(state.watchdog_capacity_waits, 2);
    }

    #[test]
    fn watchdog_without_flag_still_exhausts() {
        // Sanity: with watchdog OFF the same tiny budget DOES terminate.
        let ctl = RetryControl {
            max_retries: 3,
            watchdog: false,
            ..RetryControl::default()
        };
        let mut state = RetryState::default();
        let mut saw_terminal = false;
        for _ in 0..10u32 {
            let step = next_step(
                &mut state,
                &ctl,
                &LlmError::Overloaded { repeated: false },
                0,
            );
            if step == DriveStep::Terminal {
                saw_terminal = true;
                break;
            }
        }
        assert!(
            saw_terminal,
            "without watchdog the budget must eventually exhaust"
        );
    }

    /// Regression (review RV1): under the watchdog `max_retries` defaults to
    /// 300. A PERSISTENT non-capacity error (ProviderInternal / Transport) is
    /// NOT exempt from the budget — only 529/429 are — so it MUST terminate once
    /// `attempt` reaches the budget. Before the fix `state.attempt` was a `u8`
    /// that saturated at 255 via `saturating_add`, so the terminal check
    /// `attempt >= max_retries` (255 >= 300) never tripped and the turn retried
    /// forever. The loop is bounded above 300 so the test can never hang.
    #[test]
    fn watchdog_provider_internal_honours_budget_at_300() {
        let ctl = RetryControl {
            max_retries: WATCHDOG_MAX_RETRIES, // 300
            watchdog: true,
            ..RetryControl::default()
        };
        let mut state = RetryState::default();
        let mut terminal_at: Option<u32> = None;
        for _ in 0..(WATCHDOG_MAX_RETRIES + 20) {
            let attempt_before = state.attempt;
            let step = next_step(&mut state, &ctl, &LlmError::ProviderInternal, 0);
            if step == DriveStep::Terminal {
                terminal_at = Some(attempt_before);
                break;
            }
            assert!(
                matches!(step, DriveStep::RetryAfter(_)),
                "within budget ProviderInternal must RetryAfter, got {step:?}"
            );
        }
        assert_eq!(
            terminal_at,
            Some(WATCHDOG_MAX_RETRIES),
            "persistent ProviderInternal must terminate at the 300-attempt budget under the watchdog"
        );
    }

    /// Regression (review RV1): a `routing.retry.maxAttempts` (settings) budget
    /// above the old `u8` ceiling (255) must still terminate, not retry forever.
    #[test]
    fn provider_internal_honours_budget_above_255_from_settings() {
        let ctl = RetryControl {
            max_retries: 260,
            watchdog: false,
            ..RetryControl::default()
        };
        let mut state = RetryState::default();
        let mut terminal_at: Option<u32> = None;
        for _ in 0..(260 + 20) {
            let attempt_before = state.attempt;
            let step = next_step(
                &mut state,
                &ctl,
                &LlmError::Transport {
                    message: "connection reset".into(),
                },
                0,
            );
            if step == DriveStep::Terminal {
                terminal_at = Some(attempt_before);
                break;
            }
        }
        assert_eq!(
            terminal_at,
            Some(260),
            "Transport must terminate at a >255 settings budget, not retry forever"
        );
    }

    #[test]
    fn watchdog_capacity_backoff_caps_at_six_hours() {
        // The watchdog ladder caps at TLp=21_600_000ms, not the 32s MAX_BACKOFF_MS.
        // A large attempt saturates to the 6h cap (pre-jitter base).
        assert_eq!(
            capacity_base_delay_ms(60, None, true),
            WATCHDOG_MAX_BACKOFF_MS
        );
        assert_eq!(WATCHDOG_MAX_BACKOFF_MS, 21_600_000);
        // Without the watchdog the same attempt caps at the normal 32s.
        assert_eq!(capacity_base_delay_ms(60, None, false), MAX_BACKOFF_MS);
    }
}
