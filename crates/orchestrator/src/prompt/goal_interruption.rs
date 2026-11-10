//! `/goal` interruption handling — what an active goal does when a turn ends
//! badly (2.1.269).
//!
//! Before this, a turn that failed left the goal simply sitting there: the goal
//! evaluation runs off the Stop hook, a failed turn produces no goal
//! disposition, and nothing told the user. The CHANGELOG entry is
//! *"Fixed `/goal` runs silently stalling after API errors, network drops, or
//! token limits: the goal now retries with backoff, or pauses and says why,
//! including until a usage limit resets."*
//!
//! Upstream splits the outcome four ways (`Kps` → `{kind, cause}`):
//!
//! | kind | meaning |
//! |---|---|
//! | `none` | not the goal's business — the turn ended normally, was aborted, or hit `max_turns` |
//! | `retry` | transient — re-prompt after a backoff, up to three times |
//! | `pause` | the goal stays SET but stops driving; the user sends a message to continue |
//! | `stop` | unrecoverable — the goal is CLEARED |
//!
//! The `stop` tier already exists in this port as
//! [`crate::turn_loop::goal_clear_bucket`] (it predates the retry/pause tiers).
//! This module owns the two tiers 2.1.269 added, so the existing clear path is
//! untouched.
//!
//! The messages and classifiers follow the 2.1.322 binary
//! (`chunk-280402177.js`: `Yf`, `zf`, `Xf`, `Jf`). The separator is
//! U+00B7 MIDDLE DOT, which the binary spells `\xB7`.

/// Why a failed turn is worth retrying — oracle `qps`, the phrase interpolated
/// into the retry and gave-up announcements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryCause {
    /// `api_unavailable`
    ApiUnavailable,
    /// `unclassified`
    Unclassified,
    /// `output_limit`
    OutputLimit,
    /// `cloud_credentials`
    CloudCredentials,
    /// `host_auth`
    HostAuth,
    /// `image`
    Image,
    /// `unreadable_tool_call`
    UnreadableToolCall,
    /// `goal_check`
    GoalCheck,
}

impl RetryCause {
    /// The `qps[cause]` phrase.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::ApiUnavailable => "the API was unavailable or the connection dropped",
            Self::Unclassified => "the API returned an unexpected response",
            Self::OutputLimit => "the response hit the output token limit",
            Self::CloudCredentials => "cloud credentials did not load",
            Self::HostAuth => "sign-in was being refreshed",
            Self::Image => "an image could not be sent",
            Self::UnreadableToolCall => "Claude's tool call could not be read",
            Self::GoalCheck => "the goal check could not complete",
        }
    }
}

/// Why the goal stopped driving but stayed SET — oracle `Vps`, a complete
/// sentence (unlike [`RetryCause`], which is a fragment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseCause {
    /// `request_rejected`
    RequestRejected,
    /// `turn_error`
    TurnError,
    /// `hook_stopped`
    HookStopped,
    /// `goal_check_timeout`
    GoalCheckTimeout,
    /// `goal_check_capped`
    GoalCheckCapped,
    /// `rate_limited`
    RateLimited,
    /// `daily_cap` — organization usage is paused without a reset promise.
    DailyCap,
    /// `usage_limit` — the user must come back after the reset.
    UsageLimit,
    /// `usage_limit_waiting` — something is already waiting out the reset, so
    /// the goal resumes on its own. Upstream picks this over [`Self::UsageLimit`]
    /// when the host `hasIntent()`.
    UsageLimitWaiting,
}

impl PauseCause {
    /// The `Vps[cause]` message, shown as a notice.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::RequestRejected => {
                "Goal paused \u{b7} the API rejected the last request \u{b7} send a message to continue, or run /goal clear"
            }
            Self::TurnError => {
                "Goal paused \u{b7} the last turn could not finish \u{b7} send a message to continue"
            }
            Self::HookStopped => {
                "Goal paused \u{b7} a hook ended the turn \u{b7} send a message to continue"
            }
            Self::GoalCheckTimeout => {
                "Goal paused \u{b7} the goal check timed out \u{b7} send a message to continue"
            }
            Self::GoalCheckCapped => {
                "Goal paused \u{b7} goal checks kept finding it unmet this turn \u{b7} send a message to continue"
            }
            Self::RateLimited => {
                "Goal paused \u{b7} the request was rate limited \u{b7} send a message to retry"
            }
            Self::DailyCap => "Goal paused \u{b7} usage is paused for your organization",
            Self::UsageLimit => {
                "Goal paused \u{b7} usage limit reached \u{b7} send a message after it resets to continue"
            }
            Self::UsageLimitWaiting => {
                "Goal paused \u{b7} usage limit reached \u{b7} continues automatically when it resets"
            }
        }
    }

    /// The dedupe key upstream stores in `announced`, so the same pause is not
    /// re-announced turn after turn.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::RequestRejected => "request_rejected",
            Self::TurnError => "turn_error",
            Self::HookStopped => "hook_stopped",
            Self::GoalCheckTimeout => "goal_check_timeout",
            Self::GoalCheckCapped => "goal_check_capped",
            Self::RateLimited => "rate_limited",
            Self::DailyCap => "daily_cap",
            Self::UsageLimit => "usage_limit",
            Self::UsageLimitWaiting => "usage_limit_waiting",
        }
    }
}

/// Oracle `$St` — the backoff ladder, one entry per attempt.
pub const RETRY_BACKOFF_MS: [i64; 3] = [60_000, 300_000, 900_000];

/// Oracle `zps` — jitter fraction. The armed delay is `base * (1 + rand * 0.2)`,
/// so it only ever runs LONG, never short.
pub const RETRY_JITTER: f64 = 0.2;

/// Oracle `FSt = $St.length` — how many automatic retries before giving up.
pub const MAX_RETRIES: u32 = RETRY_BACKOFF_MS.len() as u32;

/// The armed delay for `attempt` (0-based), jittered by `rand01` ∈ [0, 1).
///
/// `Math.round(me * (1 + Math.random() * zps))` with `me = $St[M] ?? $St[0]` —
/// an attempt past the end of the ladder falls back to the FIRST rung, not the
/// last. `rand01` is a parameter so the ladder is testable without a clock or
/// an RNG.
#[must_use]
pub fn retry_delay_ms(attempt: u32, rand01: f64) -> i64 {
    let base = RETRY_BACKOFF_MS
        .get(attempt as usize)
        .copied()
        .unwrap_or(RETRY_BACKOFF_MS[0]);
    #[allow(clippy::cast_possible_truncation)]
    let jittered = (base as f64 * (1.0 + rand01.clamp(0.0, 1.0) * RETRY_JITTER)).round() as i64;
    jittered
}

/// `Goal still active \u{b7} {cause} \u{b7} retrying in {n} min ({k}/{max}) \u{b7} send a message to retry now`
///
/// `attempt` is 1-based here (upstream interpolates `M+1`). The minutes are
/// `Math.round(me / 60000)` of the BASE backoff rung; jitter affects only the timer.
#[must_use]
pub fn retry_announcement(cause: RetryCause, delay_ms: i64, attempt: u32) -> String {
    #[allow(clippy::cast_precision_loss)]
    let minutes = (delay_ms as f64 / 60_000.0).round() as i64;
    format!(
        "Goal still active \u{b7} {} \u{b7} retrying in {minutes} min ({attempt}/{MAX_RETRIES}) \u{b7} send a message to retry now",
        cause.text()
    )
}

/// `Goal paused after {max} automatic retries \u{b7} {cause} \u{b7} send a message to continue`
#[must_use]
pub fn gave_up_announcement(cause: RetryCause) -> String {
    format!(
        "Goal paused after {MAX_RETRIES} automatic retries \u{b7} {} \u{b7} send a message to continue",
        cause.text()
    )
}

/// What an active goal should do about a turn that ended badly — the retry and
/// pause tiers of oracle `Kps`. `None` means "leave it to the existing clear
/// path, or do nothing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalInterruption {
    /// Transient: re-prompt after a backoff.
    Retry(RetryCause),
    /// Stop driving, stay set, say why.
    Pause(PauseCause),
}

/// Trusted facts attached to the terminal error and its owning host. These
/// cannot be recovered from rendered error text or warning-only limit headers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApiErrorInterruptionFacts {
    /// Native `quotaLimits.dailyCapPaused === true`.
    pub daily_cap_paused: bool,
    /// Native `quotaLimits !== undefined && EA(quotaLimits)`.
    pub quota_exhausted: bool,
    /// Native host reset-wait owner already has an intent.
    pub host_waits_for_reset: bool,
    /// Native remote/host authentication path owns sign-in refresh.
    pub host_handles_auth: bool,
    /// Native `oauthRefreshDiskFull`, which is classified as authentication.
    pub oauth_refresh_disk_full: bool,
}

/// Classify this port's `model_error` turn arm (which IS the oracle's
/// `api_error` reason — see the naming note on `GoalClearBucket`) into the
/// retry/pause tiers.
///
/// Returns `None` for the arms the existing [`crate::turn_loop::goal_clear_bucket`]
/// already handles by CLEARING the goal, so the two classifiers compose without
/// either one second-guessing the other.
///
/// `quota_exhausted` is the oracle's `e.quotaLimits !== void 0 && iRe(...)` —
/// a rate limit that is really the account's usage cap, not a burst limit.
/// `host_waits_for_reset` is `u5t.of(host).hasIntent()`: something is already
/// waiting out the reset, so the goal can say it resumes on its own.
#[must_use]
pub fn classify_api_error_interruption(
    error_kind: Option<&str>,
    is_transient: bool,
    quota_exhausted: bool,
    host_waits_for_reset: bool,
) -> Option<GoalInterruption> {
    classify_api_error_interruption_with_facts(
        error_kind,
        is_transient,
        ApiErrorInterruptionFacts {
            quota_exhausted,
            host_waits_for_reset,
            ..Default::default()
        },
    )
}

/// Claude Code 2.1.322 `Xf` API-error branch, preserving its ordering. The
/// existing clear classifier owns every terminal auth/billing/context result.
#[must_use]
pub fn classify_api_error_interruption_with_facts(
    error_kind: Option<&str>,
    is_transient: bool,
    facts: ApiErrorInterruptionFacts,
) -> Option<GoalInterruption> {
    // `if(e.errorKind==="rate_limit")` comes FIRST upstream, ahead of the
    // `isTransient` shortcut, so a rate limit always pauses rather than being
    // retried as a transient blip.
    if error_kind == Some("rate_limit") {
        return Some(GoalInterruption::Pause(if facts.daily_cap_paused {
            PauseCause::DailyCap
        } else if facts.quota_exhausted {
            if facts.host_waits_for_reset {
                PauseCause::UsageLimitWaiting
            } else {
                PauseCause::UsageLimit
            }
        } else {
            PauseCause::RateLimited
        }));
    }
    if error_kind == Some("max_output_tokens") {
        return Some(GoalInterruption::Retry(RetryCause::OutputLimit));
    }
    if error_kind == Some("cloud_credential_error") {
        return Some(GoalInterruption::Retry(RetryCause::CloudCredentials));
    }
    if is_transient {
        return Some(GoalInterruption::Retry(RetryCause::ApiUnavailable));
    }
    let effective_kind = if facts.oauth_refresh_disk_full {
        Some("authentication_failed")
    } else {
        error_kind
    };
    match effective_kind {
        Some("overloaded" | "server_error") => {
            Some(GoalInterruption::Retry(RetryCause::ApiUnavailable))
        }
        // `case "unknown": case void 0:` — an error nobody classified is still
        // worth one more try, rather than silently stalling the goal.
        Some("unknown") | None => Some(GoalInterruption::Retry(RetryCause::Unclassified)),
        Some("invalid_request") => Some(GoalInterruption::Pause(PauseCause::RequestRejected)),
        Some("authentication_failed" | "oauth_org_not_allowed") if facts.host_handles_auth => {
            Some(GoalInterruption::Retry(RetryCause::HostAuth))
        }
        // `authentication_failed` / `oauth_org_not_allowed` / `account_on_hold` /
        // `verification_required` / `billing_error` / `model_not_found` all end
        // in the CLEAR tier, which `goal_clear_bucket` already owns.
        _ => None,
    }
}

/// X7n/eps model-facing retry prompt.
pub fn retry_body(
    condition: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    cause: RetryCause,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    let mut units = super::goal_checkin::goal_checkin_heading(condition)
        .string_units("")
        .unwrap_or_default();
    units.extend(
        " is still active. The last turn ended before the goal could be evaluated: ".encode_utf16(),
    );
    units.extend(cause.text().encode_utf16());
    units.extend(". Continue toward the goal.".encode_utf16());
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("generated goal retry body is a valid source string")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest_oracle() -> serde_json::Value {
        let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(include_str!(
            "../../../command-api/tests/fixtures/goal_2_1_322.json"
        ))
        .unwrap();
        let interruption = source
            .at_pointer("/interruption")
            .unwrap()
            .to_projection()
            .unwrap();
        serde_json::json!({"interruption": serde_json::from_str::<serde_json::Value>(&interruption.to_json_string().unwrap()).unwrap()})
    }

    const RETRY_CAUSES: &[(&str, RetryCause)] = &[
        ("api_unavailable", RetryCause::ApiUnavailable),
        ("unclassified", RetryCause::Unclassified),
        ("output_limit", RetryCause::OutputLimit),
        ("cloud_credentials", RetryCause::CloudCredentials),
        ("host_auth", RetryCause::HostAuth),
        ("image", RetryCause::Image),
        ("unreadable_tool_call", RetryCause::UnreadableToolCall),
        ("goal_check", RetryCause::GoalCheck),
    ];

    const PAUSE_CAUSES: &[PauseCause] = &[
        PauseCause::RequestRejected,
        PauseCause::TurnError,
        PauseCause::HookStopped,
        PauseCause::GoalCheckTimeout,
        PauseCause::GoalCheckCapped,
        PauseCause::RateLimited,
        PauseCause::DailyCap,
        PauseCause::UsageLimit,
        PauseCause::UsageLimitWaiting,
    ];

    #[test]
    fn interruption_messages_and_backoff_match_2_1_322_native_bytes() {
        let fixture = latest_oracle();
        let oracle = &fixture["interruption"];
        assert_eq!(
            oracle["retryCauses"].as_object().unwrap().len(),
            RETRY_CAUSES.len()
        );
        for &(key, cause) in RETRY_CAUSES {
            assert_eq!(
                Some(cause.text()),
                oracle["retryCauses"][key].as_str(),
                "{key}"
            );
        }
        assert_eq!(
            oracle["pauseMessages"].as_object().unwrap().len(),
            PAUSE_CAUSES.len()
        );
        for &cause in PAUSE_CAUSES {
            assert_eq!(
                Some(cause.text()),
                oracle["pauseMessages"][cause.key()].as_str(),
                "{}",
                cause.key(),
            );
        }
        assert_eq!(
            RETRY_BACKOFF_MS.to_vec(),
            oracle["backoffs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_i64().unwrap())
                .collect::<Vec<_>>(),
        );
        assert_eq!(Some(RETRY_JITTER), oracle["jitter"].as_f64());
        assert_eq!(Some(u64::from(MAX_RETRIES)), oracle["maxRetries"].as_u64());
    }

    #[test]
    fn api_retry_and_pause_tiers_match_2_1_322_native_cases() {
        let fixture = latest_oracle();
        for case in fixture["interruption"]["cases"].as_array().unwrap() {
            let input = &case["input"];
            if input["reason"].as_str() != Some("api_error") {
                continue;
            }
            let context = &case["context"];
            let actual = classify_api_error_interruption_with_facts(
                input["errorKind"].as_str(),
                input["isTransient"].as_bool().unwrap_or(false),
                ApiErrorInterruptionFacts {
                    daily_cap_paused: input["quotaLimits"]["dailyCapPaused"] == true,
                    quota_exhausted: input.get("quotaLimits").is_some()
                        && context["quotaExhausted"] == true,
                    host_waits_for_reset: false,
                    host_handles_auth: [
                        "envRemote",
                        "remoteHost",
                        "workspaceHost",
                        "inProcessRemote",
                    ]
                    .iter()
                    .any(|key| context[*key] == true),
                    oauth_refresh_disk_full: input["oauthRefreshDiskFull"] == true,
                },
            );
            let expected = &case["expected"];
            match expected["kind"].as_str().unwrap() {
                "stop" | "none" => assert_eq!(actual, None, "{}", case["name"]),
                "pause" => {
                    let expected_cause = PAUSE_CAUSES
                        .iter()
                        .find(|cause| Some(cause.key()) == expected["cause"].as_str())
                        .unwrap();
                    assert_eq!(
                        actual,
                        Some(GoalInterruption::Pause(*expected_cause)),
                        "{}",
                        case["name"],
                    );
                }
                "retry" => {
                    let &(_, expected_cause) = RETRY_CAUSES
                        .iter()
                        .find(|(key, _)| Some(*key) == expected["cause"].as_str())
                        .unwrap();
                    assert_eq!(
                        actual,
                        Some(GoalInterruption::Retry(expected_cause)),
                        "{}",
                        case["name"],
                    );
                }
                other => panic!("unsupported oracle disposition {other}"),
            }
        }
    }

    /// Every `Vps` message, byte-exact. These are the sentences a stalled goal
    /// shows instead of nothing, so a drifted one is the whole bug returning in
    /// a quieter form.
    #[test]
    fn pause_messages_match_the_oracle() {
        assert_eq!(
            PauseCause::RequestRejected.text(),
            "Goal paused · the API rejected the last request · send a message to continue, or run /goal clear"
        );
        assert_eq!(
            PauseCause::TurnError.text(),
            "Goal paused · the last turn could not finish · send a message to continue"
        );
        assert_eq!(
            PauseCause::HookStopped.text(),
            "Goal paused · a hook ended the turn · send a message to continue"
        );
        assert_eq!(
            PauseCause::GoalCheckTimeout.text(),
            "Goal paused · the goal check timed out · send a message to continue"
        );
        assert_eq!(
            PauseCause::GoalCheckCapped.text(),
            "Goal paused · goal checks kept finding it unmet this turn · send a message to continue"
        );
        assert_eq!(
            PauseCause::RateLimited.text(),
            "Goal paused · the request was rate limited · send a message to retry"
        );
        assert_eq!(
            PauseCause::DailyCap.text(),
            "Goal paused · usage is paused for your organization"
        );
        assert_eq!(
            PauseCause::UsageLimit.text(),
            "Goal paused · usage limit reached · send a message after it resets to continue"
        );
        assert_eq!(
            PauseCause::UsageLimitWaiting.text(),
            "Goal paused · usage limit reached · continues automatically when it resets"
        );
    }

    /// The separator is U+00B7 MIDDLE DOT, not an ASCII interpunct lookalike.
    #[test]
    fn the_separator_is_a_middle_dot() {
        assert!(PauseCause::TurnError.text().contains('\u{b7}'));
        assert!(!PauseCause::TurnError.text().contains('*'));
    }

    #[test]
    fn retry_phrases_match_the_oracle() {
        assert_eq!(
            RetryCause::ApiUnavailable.text(),
            "the API was unavailable or the connection dropped"
        );
        assert_eq!(
            RetryCause::Unclassified.text(),
            "the API returned an unexpected response"
        );
        assert_eq!(
            RetryCause::OutputLimit.text(),
            "the response hit the output token limit"
        );
        assert_eq!(
            RetryCause::CloudCredentials.text(),
            "cloud credentials did not load"
        );
        assert_eq!(RetryCause::HostAuth.text(), "sign-in was being refreshed");
        assert_eq!(RetryCause::Image.text(), "an image could not be sent");
        assert_eq!(
            RetryCause::UnreadableToolCall.text(),
            "Claude's tool call could not be read"
        );
        assert_eq!(
            RetryCause::GoalCheck.text(),
            "the goal check could not complete"
        );
    }

    /// `$St` / `zps` / `FSt`.
    #[test]
    fn the_backoff_ladder_matches_the_oracle() {
        assert_eq!(RETRY_BACKOFF_MS, [60_000, 300_000, 900_000]);
        assert_eq!(MAX_RETRIES, 3);
        // No jitter ⇒ exactly the rung.
        assert_eq!(retry_delay_ms(0, 0.0), 60_000);
        assert_eq!(retry_delay_ms(1, 0.0), 300_000);
        assert_eq!(retry_delay_ms(2, 0.0), 900_000);
    }

    /// Jitter only ever runs LONG — `1 + rand*0.2` has no negative branch, so a
    /// retry can never fire earlier than its rung.
    #[test]
    fn jitter_never_shortens_the_delay() {
        for attempt in 0..3u32 {
            let base = RETRY_BACKOFF_MS[attempt as usize];
            for r in [0.0, 0.25, 0.5, 0.99] {
                let d = retry_delay_ms(attempt, r);
                assert!(d >= base, "attempt {attempt} r={r}: {d} < {base}");
                assert!(
                    d <= (base as f64 * 1.2).round() as i64,
                    "attempt {attempt} r={r}: {d} exceeds +20%"
                );
            }
        }
    }

    /// `$St[M] ?? $St[0]` — past the end of the ladder it falls back to the
    /// FIRST rung, not the last. Getting this backwards would make a runaway
    /// retry wait 15 minutes instead of 1.
    #[test]
    fn an_attempt_past_the_ladder_falls_back_to_the_first_rung() {
        assert_eq!(retry_delay_ms(3, 0.0), 60_000);
        assert_eq!(retry_delay_ms(99, 0.0), 60_000);
    }

    #[test]
    fn the_announcements_match_the_oracle() {
        assert_eq!(
            retry_announcement(RetryCause::ApiUnavailable, 60_000, 1),
            "Goal still active · the API was unavailable or the connection dropped · retrying in 1 min (1/3) · send a message to retry now"
        );
        assert_eq!(
            retry_announcement(RetryCause::OutputLimit, 900_000, 3),
            "Goal still active · the response hit the output token limit · retrying in 15 min (3/3) · send a message to retry now"
        );
        assert_eq!(
            gave_up_announcement(RetryCause::Unclassified),
            "Goal paused after 3 automatic retries · the API returned an unexpected response · send a message to continue"
        );
    }

    #[test]
    fn retry_body_drops_lone_surrogates_before_display() {
        let condition = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            "fix � now".to_string(),
            vec![
                0x0066, 0x0069, 0x0078, 0x0020, 0xd800, 0x0020, 0x006e, 0x006f, 0x0077,
            ],
        )
        .unwrap();
        let body = retry_body(&condition, RetryCause::ApiUnavailable);
        let units = body.string_units("").unwrap();
        assert!(!units.contains(&0xd800));
        assert!(body.string_at("").unwrap().contains("«fix now»"));
    }

    /// A rate limit PAUSES; it is never retried as a transient blip. Upstream
    /// tests `errorKind === "rate_limit"` before the `isTransient` shortcut, so
    /// ordering is the contract here, not an implementation detail.
    #[test]
    fn a_rate_limit_pauses_even_when_marked_transient() {
        assert_eq!(
            classify_api_error_interruption(Some("rate_limit"), true, false, false),
            Some(GoalInterruption::Pause(PauseCause::RateLimited))
        );
    }

    /// A usage cap is a different sentence from a burst limit, and different
    /// again when something is already waiting out the reset.
    #[test]
    fn a_usage_cap_distinguishes_waiting_from_not() {
        assert_eq!(
            classify_api_error_interruption(Some("rate_limit"), false, true, false),
            Some(GoalInterruption::Pause(PauseCause::UsageLimit))
        );
        assert_eq!(
            classify_api_error_interruption(Some("rate_limit"), false, true, true),
            Some(GoalInterruption::Pause(PauseCause::UsageLimitWaiting))
        );
    }

    #[test]
    fn a_daily_cap_precedes_usage_reset_waiting_and_transient_retries() {
        assert_eq!(
            classify_api_error_interruption_with_facts(
                Some("rate_limit"),
                true,
                ApiErrorInterruptionFacts {
                    daily_cap_paused: true,
                    quota_exhausted: true,
                    host_waits_for_reset: true,
                    ..Default::default()
                },
            ),
            Some(GoalInterruption::Pause(PauseCause::DailyCap)),
        );
        assert_eq!(PauseCause::DailyCap.key(), "daily_cap");
    }

    #[test]
    fn host_auth_and_disk_full_facts_preserve_native_classification_order() {
        for kind in [Some("authentication_failed"), Some("oauth_org_not_allowed")] {
            assert_eq!(
                classify_api_error_interruption_with_facts(
                    kind,
                    false,
                    ApiErrorInterruptionFacts {
                        host_handles_auth: true,
                        ..Default::default()
                    },
                ),
                Some(GoalInterruption::Retry(RetryCause::HostAuth)),
            );
        }
        let disk_full = ApiErrorInterruptionFacts {
            oauth_refresh_disk_full: true,
            ..Default::default()
        };
        assert_eq!(
            classify_api_error_interruption_with_facts(Some("server_error"), false, disk_full),
            None,
            "local disk-full auth remains in the clear tier",
        );
        assert_eq!(
            classify_api_error_interruption_with_facts(Some("server_error"), true, disk_full),
            Some(GoalInterruption::Retry(RetryCause::ApiUnavailable)),
            "transient errors precede disk-full normalization",
        );
        assert_eq!(
            classify_api_error_interruption_with_facts(Some("max_output_tokens"), false, disk_full,),
            Some(GoalInterruption::Retry(RetryCause::OutputLimit)),
            "output-limit errors precede disk-full normalization",
        );
        assert_eq!(
            classify_api_error_interruption_with_facts(
                Some("invalid_request"),
                false,
                ApiErrorInterruptionFacts {
                    host_handles_auth: true,
                    ..disk_full
                },
            ),
            Some(GoalInterruption::Retry(RetryCause::HostAuth)),
        );
    }

    #[test]
    fn transient_and_server_errors_retry() {
        assert_eq!(
            classify_api_error_interruption(Some("server_error"), false, false, false),
            Some(GoalInterruption::Retry(RetryCause::ApiUnavailable))
        );
        assert_eq!(
            classify_api_error_interruption(Some("overloaded"), false, false, false),
            Some(GoalInterruption::Retry(RetryCause::ApiUnavailable))
        );
        assert_eq!(
            classify_api_error_interruption(Some("invalid_request"), true, false, false),
            Some(GoalInterruption::Retry(RetryCause::ApiUnavailable)),
            "isTransient wins over the invalid_request pause, as upstream orders it"
        );
    }

    /// An unclassified error retries rather than stalling — that IS the bug.
    #[test]
    fn an_unknown_error_retries_rather_than_stalling() {
        assert_eq!(
            classify_api_error_interruption(None, false, false, false),
            Some(GoalInterruption::Retry(RetryCause::Unclassified))
        );
        assert_eq!(
            classify_api_error_interruption(Some("unknown"), false, false, false),
            Some(GoalInterruption::Retry(RetryCause::Unclassified))
        );
    }

    #[test]
    fn a_rejected_request_pauses() {
        assert_eq!(
            classify_api_error_interruption(Some("invalid_request"), false, false, false),
            Some(GoalInterruption::Pause(PauseCause::RequestRejected))
        );
    }

    /// The clear-tier kinds stay `None` here, so this classifier and
    /// `goal_clear_bucket` never both claim the same turn.
    #[test]
    fn the_clear_tier_is_left_to_the_existing_classifier() {
        for kind in [
            "authentication_failed",
            "oauth_org_not_allowed",
            "account_on_hold",
            "verification_required",
            "billing_error",
            "model_not_found",
        ] {
            assert_eq!(
                classify_api_error_interruption(Some(kind), false, false, false),
                None,
                "{kind} belongs to the clear tier"
            );
        }
    }
}
