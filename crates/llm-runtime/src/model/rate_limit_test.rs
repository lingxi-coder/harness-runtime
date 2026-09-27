//! Tests for `rate_limit.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod rate_limit_test;`.

pub use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn h(k: &str, v: &str) -> Vec<(String, String)> {
        vec![(k.into(), v.into())]
    }

    #[test]
    fn rate_limit_err_fmt_is_byte_locked() {
        assert_eq!(RATE_LIMIT_ERR_FMT, "Rate limited; retrying in {N}s");
        assert_eq!(format_rate_limited_msg(7), "Rate limited; retrying in 7s");
    }

    #[test]
    fn retry_after_seconds_form_parses() {
        let r = parse_retry_after(&h("Retry-After", "5"));
        assert_eq!(r, Some(Duration::from_secs(5)));
    }

    #[test]
    fn unified_reset_future_epoch_yields_delay() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        // reset 60s in the future (epoch seconds 1_000_060)
        let r = parse_unified_reset(&h("anthropic-ratelimit-unified-reset", "1000060"), now);
        assert_eq!(r, Some(Duration::from_secs(60)));
    }

    #[test]
    fn unified_reset_past_or_now_is_none() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        // past → None (fall through, NOT zero)
        assert_eq!(
            parse_unified_reset(&h("anthropic-ratelimit-unified-reset", "999000"), now),
            None
        );
        // exactly now → None
        assert_eq!(
            parse_unified_reset(&h("anthropic-ratelimit-unified-reset", "1000000"), now),
            None
        );
    }

    #[test]
    fn unified_reset_clamps_to_six_hours() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        // reset a year out → capped at 6h
        let r = parse_unified_reset(&h("anthropic-ratelimit-unified-reset", "1031536000"), now);
        assert_eq!(r, Some(Duration::from_millis(PERSISTENT_RESET_CAP_MS)));
    }

    #[test]
    fn unified_reset_non_numeric_and_missing_are_none() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            parse_unified_reset(&h("anthropic-ratelimit-unified-reset", "soon"), now),
            None
        );
        assert_eq!(parse_unified_reset(&[], now), None);
    }

    #[test]
    fn overage_disabled_reason_reads_header() {
        assert_eq!(
            overage_disabled_reason(&h(
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "spend_limit"
            )),
            Some("spend_limit")
        );
        assert_eq!(overage_disabled_reason(&[]), None);
    }

    #[test]
    fn go_duration_forms_parse() {
        assert_eq!(parse_go_duration("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("880ms"), Some(Duration::from_millis(880)));
        assert_eq!(
            parse_go_duration("1h2m3s"),
            Some(Duration::from_secs(3600 + 120 + 3))
        );
        assert_eq!(parse_go_duration("60"), Some(Duration::from_secs(60)));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("soon"), None);
    }

    #[test]
    fn openai_reset_waits_for_binding_bucket() {
        let headers = vec![
            ("x-ratelimit-reset-requests".to_string(), "1s".to_string()),
            ("x-ratelimit-reset-tokens".to_string(), "6m0s".to_string()),
        ];
        // Wait the longer (tokens) bucket, not the 1s requests one.
        assert_eq!(parse_openai_reset(&headers), Some(Duration::from_secs(360)));
        // Only one present.
        assert_eq!(
            parse_openai_reset(&h("x-ratelimit-reset-tokens", "30s")),
            Some(Duration::from_secs(30))
        );
        // None present (Anthropic-only response) → None, so the 1s fallback path
        // in resolve_retry_after is unchanged for Anthropic.
        assert_eq!(parse_openai_reset(&h("retry-after", "5")), None);
    }

    #[test]
    fn retry_after_case_insensitive_lookup() {
        let r = parse_retry_after(&h("retry-after", "12"));
        assert_eq!(r, Some(Duration::from_secs(12)));
    }

    #[test]
    fn retry_after_http_date_form_rejected() {
        let r = parse_retry_after(&h("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT"));
        assert_eq!(r, None);
    }

    #[test]
    fn retry_after_missing_returns_none() {
        let r = parse_retry_after(&[]);
        assert_eq!(r, None);
    }

    #[test]
    fn anthropic_ratelimit_reset_parses_future_iso8601() {
        // 2026-05-23T12:00:00Z relative to 2026-05-23T11:59:50Z should be 10s.
        let now = SystemTime::UNIX_EPOCH
            + Duration::from_secs(parse_iso8601_utc("2026-05-23T11:59:50Z").unwrap());
        let r = parse_anthropic_ratelimit_reset(
            &h("anthropic-ratelimit-requests-reset", "2026-05-23T12:00:00Z"),
            now,
        );
        assert_eq!(r, Some(Duration::from_secs(10)));
    }

    #[test]
    fn anthropic_ratelimit_reset_past_clamps_to_zero() {
        let now = SystemTime::UNIX_EPOCH
            + Duration::from_secs(parse_iso8601_utc("2026-05-23T13:00:00Z").unwrap());
        let r = parse_anthropic_ratelimit_reset(
            &h("anthropic-ratelimit-requests-reset", "2026-05-23T12:00:00Z"),
            now,
        );
        assert_eq!(r, Some(Duration::ZERO));
    }

    #[test]
    fn anthropic_ratelimit_reset_malformed_returns_none() {
        let r = parse_anthropic_ratelimit_reset(
            &h("anthropic-ratelimit-requests-reset", "not-an-iso-date"),
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(r, None);
    }

    #[test]
    fn iso8601_parser_handles_2026_05_23_correctly() {
        // 2026-05-23T00:00:00Z must round-trip to a positive Unix epoch
        // and be exactly 56 years × 365.25 days × 86400 sec ≈ 1.77 * 10^9.
        let secs = parse_iso8601_utc("2026-05-23T00:00:00Z").unwrap();
        assert!(secs > 1_700_000_000 && secs < 1_900_000_000);
    }

    // ---- adapter tests (Step 2) ----

    #[test]
    fn retry_secs_from_rate_limited_7s() {
        let err = crate::LlmError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
            scope: None,
        };
        assert_eq!(retry_secs_from_error(&err), Some(7));
    }

    #[test]
    fn retry_secs_from_rate_limited_zero_ms_clamps_to_1() {
        let err = crate::LlmError::RateLimited {
            retry_after: Some(Duration::from_millis(0)),
            scope: None,
        };
        // 0ms → as_secs() == 0, max(1) → Some(1)
        assert_eq!(retry_secs_from_error(&err), Some(1));
    }

    #[test]
    fn retry_secs_from_rate_limited_none_retry_after() {
        let err = crate::LlmError::RateLimited {
            retry_after: None,
            scope: None,
        };
        assert_eq!(retry_secs_from_error(&err), None);
    }

    #[test]
    fn retry_secs_from_non_rate_limit_error_is_none() {
        assert_eq!(
            retry_secs_from_error(&crate::LlmError::ProviderInternal),
            None
        );
        assert_eq!(
            retry_secs_from_error(&crate::LlmError::Authentication {
                message: String::new()
            }),
            None
        );
        assert_eq!(
            retry_secs_from_error(&crate::LlmError::Overloaded { repeated: false }),
            None
        );
    }

    #[test]
    fn format_rate_limited_msg_7_byte_locked() {
        // The byte-locked template output for secs=7.
        assert_eq!(format_rate_limited_msg(7), "Rate limited; retrying in 7s");
    }
}

#[cfg(test)]
mod format_reset_time_tests {
    //! Byte-faithful tests for the `formatResetTime` port. `now` and the reset
    //! instant are both built from **local** wall-clock components and injected
    //! into `format_reset_time_at`, so the assertions are independent of the
    //! runner's timezone: the function re-derives local Y/M/D/H/M from the epoch
    //! it is given, which round-trips the components we constructed. The tz
    //! suffix (`reset_time_zone`) renders the host IANA zone name via
    //! `iana-time-zone` (byte-faithful to TS `getTimeZone()`), which is
    //! host-dependent — so the showTimezone tests assert the structural ` (…)`
    //! shape, not a literal zone string.
    use super::*;

    /// A `DateTime<Local>` for the given local wall-clock components. Tests pass
    /// `.timestamp()` of this into the formatter; `format_reset_time_at` then
    /// rebuilds the same local components, so the rendered output matches what
    /// these inputs describe regardless of the machine timezone.
    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    fn fmt(
        reset: DateTime<Local>,
        now: DateTime<Local>,
        show_tz: bool,
        show_time: bool,
    ) -> Option<String> {
        format_reset_time_at(Some(reset.timestamp()), now, show_tz, show_time)
    }

    #[test]
    fn none_or_zero_timestamp_returns_none() {
        let now = local(2026, 6, 5, 12, 0);
        assert_eq!(format_reset_time_at(None, now, false, true), None);
        // TS `if (!timestampInSeconds)` treats 0 as falsy.
        assert_eq!(format_reset_time_at(Some(0), now, false, true), None);
    }

    #[test]
    fn within_24h_same_minute_zero_drops_minutes() {
        // 3:00pm, ~3h out → "3pm".
        let now = local(2026, 6, 5, 12, 0);
        let reset = local(2026, 6, 5, 15, 0);
        assert_eq!(fmt(reset, now, false, true).as_deref(), Some("3pm"));
    }

    #[test]
    fn within_24h_with_minutes_renders_colon_minutes() {
        // 3:30pm, ~3.5h out → "3:30pm".
        let now = local(2026, 6, 5, 12, 0);
        let reset = local(2026, 6, 5, 15, 30);
        assert_eq!(fmt(reset, now, false, true).as_deref(), Some("3:30pm"));
    }

    #[test]
    fn within_24h_midnight_and_noon_edges() {
        // 12am (midnight) and 12pm (noon) — hour12 = 12, not 0.
        let now = local(2026, 6, 5, 23, 0);
        let midnight = local(2026, 6, 6, 0, 0);
        assert_eq!(fmt(midnight, now, false, true).as_deref(), Some("12am"));

        let now2 = local(2026, 6, 5, 10, 0);
        let noon = local(2026, 6, 5, 12, 5);
        assert_eq!(fmt(noon, now2, false, true).as_deref(), Some("12:05pm"));
    }

    #[test]
    fn over_24h_same_year_includes_month_day_and_time() {
        // Reset Jun 7 3:30pm, now Jun 5 → >24h, same year → "Jun 7, 3:30pm".
        let now = local(2026, 6, 5, 10, 0);
        let reset = local(2026, 6, 7, 15, 30);
        assert_eq!(
            fmt(reset, now, false, true).as_deref(),
            Some("Jun 7, 3:30pm")
        );
    }

    #[test]
    fn over_24h_same_year_minute_zero_drops_minutes() {
        // Reset Jun 7 3:00pm → "Jun 7, 3pm".
        let now = local(2026, 6, 5, 10, 0);
        let reset = local(2026, 6, 7, 15, 0);
        assert_eq!(fmt(reset, now, false, true).as_deref(), Some("Jun 7, 3pm"));
    }

    #[test]
    fn over_24h_different_year_includes_year() {
        // Reset 2027 → year differs → "Jun 7, 2027, 3:30pm".
        let now = local(2026, 6, 5, 10, 0);
        let reset = local(2027, 6, 7, 15, 30);
        assert_eq!(
            fmt(reset, now, false, true).as_deref(),
            Some("Jun 7, 2027, 3:30pm")
        );
    }

    #[test]
    fn over_24h_show_time_false_drops_time() {
        // showTime=false on the far-future branch → "Jun 7" (no time at all).
        let now = local(2026, 6, 5, 10, 0);
        let reset = local(2026, 6, 7, 15, 30);
        assert_eq!(fmt(reset, now, false, false).as_deref(), Some("Jun 7"));
    }

    #[test]
    fn over_24h_different_year_show_time_false_keeps_year() {
        let now = local(2026, 6, 5, 10, 0);
        let reset = local(2027, 6, 7, 15, 30);
        assert_eq!(
            fmt(reset, now, false, false).as_deref(),
            Some("Jun 7, 2027")
        );
    }

    #[test]
    fn ampm_is_lowercased_with_no_space() {
        // Explicitly assert the AM/PM lowercasing + space removal: never "PM",
        // never " pm".
        let now = local(2026, 6, 5, 1, 0);
        let am = local(2026, 6, 5, 9, 15);
        let s = fmt(am, now, false, true).unwrap();
        assert_eq!(s, "9:15am");
        assert!(!s.contains("AM") && !s.contains("PM") && !s.contains(' '));
    }

    #[test]
    fn show_timezone_appends_parenthesised_suffix() {
        // Date/time portion is byte-faithful; the tz suffix is the chrono %Z
        // offset (documented divergence), so assert the structural shape: the
        // base string, a single space, then a non-empty "(…)".
        let now = local(2026, 6, 5, 12, 0);
        let reset = local(2026, 6, 5, 15, 30);
        let off = fmt(reset, now, false, true).unwrap();
        assert_eq!(off, "3:30pm");

        let on = fmt(reset, now, true, true).unwrap();
        assert!(on.starts_with("3:30pm ("), "got {on}");
        assert!(on.ends_with(')'), "got {on}");
        // The suffix is exactly " (<tz>)" with a non-empty tz.
        let suffix = on.strip_prefix("3:30pm ").unwrap();
        assert!(suffix.len() > 2, "tz suffix should be non-empty: {suffix}");
    }

    #[test]
    fn show_timezone_on_far_future_branch_appends_suffix_after_time() {
        let now = local(2026, 6, 5, 10, 0);
        let reset = local(2026, 6, 7, 15, 30);
        let on = fmt(reset, now, true, true).unwrap();
        assert!(on.starts_with("Jun 7, 3:30pm ("), "got {on}");
        assert!(on.ends_with(')'), "got {on}");
    }

    #[test]
    fn exactly_24h_boundary_uses_time_only_branch() {
        // hoursUntilReset must be strictly > 24 for the date branch. Exactly 24h
        // stays on the time-only branch (TS `hoursUntilReset > 24`).
        let now = local(2026, 6, 5, 15, 30);
        let reset = local(2026, 6, 6, 15, 30); // exactly 24h
        assert_eq!(fmt(reset, now, false, true).as_deref(), Some("3:30pm"));

        // One minute past 24h flips to the date branch.
        let reset_past = local(2026, 6, 6, 15, 31);
        assert_eq!(
            fmt(reset_past, now, false, true).as_deref(),
            Some("Jun 6, 3:31pm")
        );
    }

    #[test]
    fn formatted_reset_times_from_headers_maps_both_windows() {
        let now = local(2026, 6, 5, 10, 0);
        let resets_at = local(2026, 6, 5, 13, 0).timestamp(); // 1pm, within 24h
        let overage_at = local(2026, 6, 7, 9, 0).timestamp(); // Jun 7 9am, >24h
        let headers = vec![
            (
                "anthropic-ratelimit-unified-reset".into(),
                resets_at.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-overage-reset".into(),
                overage_at.to_string(),
            ),
        ];
        // `formatted_reset_times_*` calls the formatter with showTimezone=true
        // (matching TS `formatResetTime(resetsAt, true)`), so each string carries
        // the tz suffix. The date/time prefix is byte-faithful; the parenthesised
        // suffix is the chrono %Z offset (documented divergence), so assert the
        // prefix + shape rather than a literal zone.
        let f = formatted_reset_times_at(&headers, now);
        let rt = f.reset_time.as_deref().unwrap();
        assert!(rt.starts_with("1pm ("), "got {rt}");
        assert!(rt.ends_with(')'), "got {rt}");
        let ort = f.overage_reset_time.as_deref().unwrap();
        assert!(ort.starts_with("Jun 7, 9am ("), "got {ort}");
        assert!(ort.ends_with(')'), "got {ort}");
        // resetsAt (1pm today) < overageResetsAt (Jun 7) → earlier is the primary.
        assert_eq!(f.reset_is_earlier, Some(true));

        // The borrowed view threads straight into the message template, carrying
        // the formatted reset string (with its tz suffix) into ` · resets …`.
        let info = RateLimitInfo {
            rate_limit_type: Some("five_hour".into()),
            overage_status: None,
            overage_disabled_reason: None,
            ..RateLimitInfo::default()
        };
        let msg =
            rate_limit_error_message(&info, &f.as_reset_times(), SubscriptionContext::default())
                .unwrap();
        assert!(
            msg.starts_with("You've hit your session limit · resets 1pm ("),
            "got {msg}"
        );
        assert!(msg.ends_with(')'), "got {msg}");
    }

    #[test]
    fn formatted_reset_times_absent_or_non_numeric_headers_yield_none() {
        let now = local(2026, 6, 5, 10, 0);
        // Missing both → all None (Number(undefined) → NaN → undefined).
        let f = formatted_reset_times_at(&[], now);
        assert_eq!(f.reset_time, None);
        assert_eq!(f.overage_reset_time, None);
        assert_eq!(f.reset_is_earlier, None);

        // Non-numeric `reset` header → None (Number("soon") → NaN).
        let headers = vec![("anthropic-ratelimit-unified-reset".into(), "soon".into())];
        let f2 = formatted_reset_times_at(&headers, now);
        assert_eq!(f2.reset_time, None);
        assert_eq!(f2.reset_is_earlier, None);
    }

    #[test]
    fn formatted_reset_times_overage_only_picks_overage_window() {
        let now = local(2026, 6, 5, 10, 0);
        let overage_at = local(2026, 6, 5, 14, 0).timestamp(); // 2pm
        let headers = vec![(
            "anthropic-ratelimit-unified-overage-reset".into(),
            overage_at.to_string(),
        )];
        let f = formatted_reset_times_at(&headers, now);
        assert_eq!(f.reset_time, None);
        // showTimezone=true → "2pm (<offset>)"; assert prefix/shape.
        let ort = f.overage_reset_time.as_deref().unwrap();
        assert!(ort.starts_with("2pm ("), "got {ort}");
        assert!(ort.ends_with(')'), "got {ort}");
        // Only one timestamp present → comparison is undefined → None.
        assert_eq!(f.reset_is_earlier, None);
    }
}

#[cfg(test)]
mod rate_limit_message {
    //! Byte-faithful tests for the 429 user-facing message
    //! (`rate_limit_error_message` / `RateLimitInfo`), named so the cargo filter
    //! `rate_limit::rate_limit_message` matches them all. Templates are locked
    //! against claude-code `rateLimitMessages.ts`.
    use super::*;

    fn info(rate_limit_type: Option<&str>, overage_status: Option<&str>) -> RateLimitInfo {
        RateLimitInfo {
            rate_limit_type: rate_limit_type.map(str::to_string),
            overage_status: overage_status.map(str::to_string),
            overage_disabled_reason: None,
            ..RateLimitInfo::default()
        }
    }

    #[test]
    fn from_headers_reads_unified_headers() {
        let headers = vec![
            (
                "anthropic-ratelimit-unified-representative-claim".into(),
                "five_hour".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason".into(),
                "out_of_credits".into(),
            ),
        ];
        let parsed = RateLimitInfo::from_headers(&headers);
        assert_eq!(parsed.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(parsed.overage_status.as_deref(), Some("rejected"));
        assert_eq!(
            parsed.overage_disabled_reason.as_deref(),
            Some("out_of_credits")
        );
        assert!(parsed.has_unified_headers());
        assert!(!RateLimitInfo::default().has_unified_headers());
    }

    #[test]
    fn parses_206_overage_headers() {
        // 2.1.206 `ClaudeAILimits` overage fields — no `status` header, so the
        // early-warning replacement in `from_headers_at` never fires and the
        // raw parse passes through untouched (same guard as
        // `from_headers_reads_unified_headers` above).
        let headers = vec![
            (
                "anthropic-ratelimit-unified-overage-in-use".into(),
                "true".into(),
            ),
            (
                "anthropic-ratelimit-unified-upgrade-paths".into(),
                "upgrade_plan, overage".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-period-monthly-utilization".into(),
                "0.42".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-period-channel-utilization".into(),
                "0.10".into(),
            ),
        ];
        let info = RateLimitInfo::from_headers(&headers);
        assert!(info.overage_in_use);
        assert_eq!(
            info.upgrade_paths.as_deref(),
            Some(&["upgrade_plan".to_string(), "overage".to_string()][..])
        );
        assert_eq!(info.overage_period_monthly_utilization, Some(0.42));
        assert_eq!(info.overage_period_channel_utilization, Some(0.10));
    }

    #[test]
    fn overage_206_fields_absent_headers_yield_defaults() {
        let info = RateLimitInfo::from_headers(&[]);
        assert!(!info.overage_in_use);
        assert_eq!(info.upgrade_paths, None);
        assert_eq!(info.overage_period_monthly_utilization, None);
        assert_eq!(info.overage_period_channel_utilization, None);
    }

    #[test]
    fn upgrade_paths_empty_header_value_yields_none() {
        // JS `d ? d.split(',')… : undefined` — an empty header value is falsy,
        // so a present-but-empty header collapses to `None` (NOT `Some([""])`),
        // same as an absent header.
        let headers = vec![(
            "anthropic-ratelimit-unified-upgrade-paths".into(),
            String::new(),
        )];
        let info = RateLimitInfo::from_headers(&headers);
        assert_eq!(info.upgrade_paths, None);
    }

    #[test]
    fn five_hour_session_limit_message_is_byte_locked() {
        // claude-code rateLimitMessages.ts:192-193 + :343.
        let msg = rate_limit_error_message(
            &info(Some("five_hour"), None),
            &ResetTimes {
                reset_time: Some("3pm"),
                ..ResetTimes::default()
            },
            SubscriptionContext::default(),
        );
        assert_eq!(
            msg.as_deref(),
            Some("You've hit your session limit · resets 3pm")
        );
    }

    #[test]
    fn five_hour_without_reset_time_omits_reset_clause() {
        let msg = rate_limit_error_message(
            &info(Some("five_hour"), None),
            &ResetTimes::default(),
            SubscriptionContext::default(),
        );
        assert_eq!(msg.as_deref(), Some("You've hit your session limit"));
    }

    #[test]
    fn seven_day_weekly_and_opus_messages_are_byte_locked() {
        let weekly = rate_limit_error_message(
            &info(Some("seven_day"), None),
            &ResetTimes::default(),
            SubscriptionContext::default(),
        );
        assert_eq!(weekly.as_deref(), Some("You've hit your weekly limit"));

        let opus = rate_limit_error_message(
            &info(Some("seven_day_opus"), None),
            &ResetTimes::default(),
            SubscriptionContext::default(),
        );
        assert_eq!(opus.as_deref(), Some("You've hit your Opus limit"));
    }

    #[test]
    fn seven_day_sonnet_wording_depends_on_subscription() {
        // Non pro/enterprise → "Sonnet limit" (rateLimitMessages.ts:180).
        let standard = rate_limit_error_message(
            &info(Some("seven_day_sonnet"), None),
            &ResetTimes::default(),
            SubscriptionContext {
                is_pro_or_enterprise: false,
            },
        );
        assert_eq!(standard.as_deref(), Some("You've hit your Sonnet limit"));

        // Pro/enterprise → "weekly limit" (rateLimitMessages.ts:178-181).
        let pro = rate_limit_error_message(
            &info(Some("seven_day_sonnet"), None),
            &ResetTimes::default(),
            SubscriptionContext {
                is_pro_or_enterprise: true,
            },
        );
        assert_eq!(pro.as_deref(), Some("You've hit your weekly limit"));
    }

    #[test]
    fn unknown_or_absent_rate_limit_type_falls_back_to_usage_limit() {
        // claude-code rateLimitMessages.ts:196 — default `usage limit`.
        let msg = rate_limit_error_message(
            &info(None, None),
            &ResetTimes::default(),
            SubscriptionContext::default(),
        );
        assert_eq!(msg.as_deref(), Some("You've hit your usage limit"));
    }

    #[test]
    fn overage_rejected_out_of_credits_message_is_byte_locked() {
        // claude-code rateLimitMessages.ts:168-170.
        let limits = RateLimitInfo {
            rate_limit_type: Some("five_hour".into()),
            overage_status: Some("rejected".into()),
            overage_disabled_reason: Some("out_of_credits".into()),
            ..RateLimitInfo::default()
        };
        let msg = rate_limit_error_message(
            &limits,
            &ResetTimes {
                overage_reset_time: Some("Jun 7, 9am"),
                ..ResetTimes::default()
            },
            SubscriptionContext::default(),
        );
        assert_eq!(
            msg.as_deref(),
            Some("You're out of extra usage · resets Jun 7, 9am")
        );
    }

    #[test]
    fn overage_rejected_other_reason_uses_limit_wording() {
        // claude-code rateLimitMessages.ts:172 — formatLimitReachedText('limit', …).
        let limits = RateLimitInfo {
            rate_limit_type: Some("seven_day".into()),
            overage_status: Some("rejected".into()),
            overage_disabled_reason: None,
            ..RateLimitInfo::default()
        };
        let msg = rate_limit_error_message(
            &limits,
            &ResetTimes {
                reset_time: Some("3pm"),
                ..ResetTimes::default()
            },
            SubscriptionContext::default(),
        );
        // The overage-rejected branch outranks rate_limit_type wording.
        assert_eq!(msg.as_deref(), Some("You've hit your limit · resets 3pm"));
    }

    #[test]
    fn overage_rejected_dual_reset_picks_earlier_window() {
        // claude-code rateLimitMessages.ts:154-166 — both present, earlier wins.
        let limits = RateLimitInfo {
            rate_limit_type: Some("seven_day".into()),
            overage_status: Some("rejected".into()),
            overage_disabled_reason: None,
            ..RateLimitInfo::default()
        };
        // resetsAt is the earlier window → use its formatted string.
        let earlier_primary = rate_limit_error_message(
            &limits,
            &ResetTimes {
                reset_time: Some("3pm"),
                overage_reset_time: Some("Jun 9, 9am"),
                reset_is_earlier: Some(true),
            },
            SubscriptionContext::default(),
        );
        assert_eq!(
            earlier_primary.as_deref(),
            Some("You've hit your limit · resets 3pm")
        );

        // overageResetsAt is the earlier window → use the overage string.
        let earlier_overage = rate_limit_error_message(
            &limits,
            &ResetTimes {
                reset_time: Some("Jun 9, 9am"),
                overage_reset_time: Some("3pm"),
                reset_is_earlier: Some(false),
            },
            SubscriptionContext::default(),
        );
        assert_eq!(
            earlier_overage.as_deref(),
            Some("You've hit your limit · resets 3pm")
        );
    }
}

#[cfg(test)]
mod unified_header_parse {
    //! Tests for the additive `RateLimitInfo` unified-header extension
    //! (`status` / `resets_at` / `utilization` / `claim_resets_at` /
    //! `overage_resets_at` / `fallback_available`), pinned against claude-code
    //! `computeNewLimitsFromHeaders` (`claudeAiLimits.ts:376-436`) and the
    //! claim→abbrev associations in `extractRawUtilization` /
    //! `EARLY_WARNING_CLAIM_MAP` (`claudeAiLimits.ts:73-77`, `:164-179`).
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn full_header_set_parses_every_field() {
        let headers = hdrs(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "1750000000"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-5h-utilization", "0.92"),
            ("anthropic-ratelimit-unified-5h-reset", "1750000100"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-reset", "1750000200"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "out_of_credits",
            ),
            ("anthropic-ratelimit-unified-fallback", "available"),
        ]);
        let p = RateLimitInfo::from_headers(&headers);
        // Existing three fields — behaviour unchanged.
        assert_eq!(p.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(p.overage_status.as_deref(), Some("allowed"));
        assert_eq!(p.overage_disabled_reason.as_deref(), Some("out_of_credits"));
        // New additive fields.
        assert_eq!(p.status.as_deref(), Some("rejected"));
        assert_eq!(p.resets_at, Some(1_750_000_000));
        assert_eq!(p.utilization, Some(0.92));
        assert_eq!(p.claim_resets_at, Some(1_750_000_100));
        assert_eq!(p.overage_resets_at, Some(1_750_000_200));
        assert_eq!(p.fallback_available, Some(true));
    }

    #[test]
    fn no_headers_yield_all_none() {
        let p = RateLimitInfo::from_headers(&[]);
        assert_eq!(p, RateLimitInfo::default());
        assert_eq!(p.status, None);
        assert_eq!(p.resets_at, None);
        assert_eq!(p.utilization, None);
        assert_eq!(p.claim_resets_at, None);
        assert_eq!(p.overage_resets_at, None);
        assert_eq!(p.fallback_available, None);
    }

    #[test]
    fn partial_headers_leave_missing_fields_none() {
        // Only the representative claim — no per-claim headers, no status.
        let headers = hdrs(&[(
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day",
        )]);
        let p = RateLimitInfo::from_headers(&headers);
        assert_eq!(p.rate_limit_type.as_deref(), Some("seven_day"));
        assert_eq!(p.status, None);
        assert_eq!(p.resets_at, None);
        assert_eq!(p.utilization, None);
        assert_eq!(p.claim_resets_at, None);
        assert_eq!(p.overage_resets_at, None);
        assert_eq!(p.fallback_available, None);
    }

    #[test]
    fn empty_status_header_is_none_like_ts_falsy_default() {
        // TS: `headers.get('anthropic-ratelimit-unified-status') || 'allowed'`
        // (claudeAiLimits.ts:379-381) — an empty string is falsy, so the header
        // value is discarded. Our `None` is that "no usable value" state.
        let headers = hdrs(&[("anthropic-ratelimit-unified-status", "")]);
        assert_eq!(RateLimitInfo::from_headers(&headers).status, None);
    }

    #[test]
    fn malformed_utilization_is_none() {
        for bad in ["abc", "", "NaN", "inf"] {
            let headers = hdrs(&[
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "five_hour",
                ),
                ("anthropic-ratelimit-unified-5h-utilization", bad),
                ("anthropic-ratelimit-unified-5h-reset", "1750000100"),
            ]);
            let p = RateLimitInfo::from_headers(&headers);
            assert_eq!(p.utilization, None, "utilization {bad:?} should be None");
            // The sibling reset header still parses on its own.
            assert_eq!(p.claim_resets_at, Some(1_750_000_100));
        }
    }

    #[test]
    fn malformed_resets_are_none() {
        let headers = hdrs(&[
            ("anthropic-ratelimit-unified-reset", "soon"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-5h-reset", ""),
            ("anthropic-ratelimit-unified-overage-reset", "-5"),
        ]);
        let p = RateLimitInfo::from_headers(&headers);
        assert_eq!(p.resets_at, None);
        assert_eq!(p.claim_resets_at, None);
        assert_eq!(p.overage_resets_at, None);
    }

    /// Per-claim headers for BOTH known windows; which one is read must follow
    /// the representative claim's abbrev.
    fn both_window_headers(claim: &str) -> Vec<(String, String)> {
        hdrs(&[
            ("anthropic-ratelimit-unified-representative-claim", claim),
            ("anthropic-ratelimit-unified-5h-utilization", "0.55"),
            ("anthropic-ratelimit-unified-5h-reset", "1750000005"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.77"),
            ("anthropic-ratelimit-unified-7d-reset", "1750000007"),
        ])
    }

    #[test]
    fn five_hour_claim_reads_5h_window() {
        let p = RateLimitInfo::from_headers(&both_window_headers("five_hour"));
        assert_eq!(p.utilization, Some(0.55));
        assert_eq!(p.claim_resets_at, Some(1_750_000_005));
    }

    #[test]
    fn seven_day_claim_reads_7d_window() {
        let p = RateLimitInfo::from_headers(&both_window_headers("seven_day"));
        assert_eq!(p.utilization, Some(0.77));
        assert_eq!(p.claim_resets_at, Some(1_750_000_007));
    }

    #[test]
    fn opus_and_sonnet_claims_have_no_per_claim_headers() {
        // claude-code has NO per-claim headers for seven_day_opus /
        // seven_day_sonnet (mockRateLimits.ts:33-40 lists only 5h/7d/overage
        // variants), so no utilization is ever attached to those claims.
        for claim in ["seven_day_opus", "seven_day_sonnet"] {
            let p = RateLimitInfo::from_headers(&both_window_headers(claim));
            assert_eq!(p.rate_limit_type.as_deref(), Some(claim));
            assert_eq!(p.utilization, None, "claim {claim} must not read windows");
            assert_eq!(p.claim_resets_at, None);
        }
    }

    #[test]
    fn unknown_claim_reads_no_per_claim_headers() {
        let p = RateLimitInfo::from_headers(&both_window_headers("lunar_month"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("lunar_month"));
        assert_eq!(p.utilization, None);
        assert_eq!(p.claim_resets_at, None);
    }

    #[test]
    fn overage_claim_reads_overage_window() {
        // EARLY_WARNING_CLAIM_MAP maps 'overage' → overage
        // (claudeAiLimits.ts:76); its per-claim headers are
        // `…-overage-utilization` / `…-overage-reset` (mockRateLimits.ts:27,39).
        let headers = hdrs(&[
            (
                "anthropic-ratelimit-unified-representative-claim",
                "overage",
            ),
            ("anthropic-ratelimit-unified-overage-utilization", "0.33"),
            ("anthropic-ratelimit-unified-overage-reset", "1750000009"),
        ]);
        let p = RateLimitInfo::from_headers(&headers);
        assert_eq!(p.utilization, Some(0.33));
        assert_eq!(p.claim_resets_at, Some(1_750_000_009));
        // The same header doubles as the overage window reset.
        assert_eq!(p.overage_resets_at, Some(1_750_000_009));
    }

    #[test]
    fn per_claim_headers_without_representative_claim_are_ignored() {
        let headers = hdrs(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.55"),
            ("anthropic-ratelimit-unified-5h-reset", "1750000005"),
        ]);
        let p = RateLimitInfo::from_headers(&headers);
        assert_eq!(p.utilization, None);
        assert_eq!(p.claim_resets_at, None);
    }

    #[test]
    fn claim_abbrev_mapping_is_pinned() {
        assert_eq!(claim_abbrev("five_hour"), Some("5h"));
        assert_eq!(claim_abbrev("seven_day"), Some("7d"));
        assert_eq!(claim_abbrev("overage"), Some("overage"));
        assert_eq!(claim_abbrev("seven_day_opus"), None);
        assert_eq!(claim_abbrev("seven_day_sonnet"), None);
        assert_eq!(claim_abbrev("lunar_month"), None);
        assert_eq!(claim_abbrev(""), None);
    }

    #[test]
    fn fallback_absent_available_and_other_values() {
        // Absent → None (TS collapses to `false`; `unwrap_or(false)` restores
        // the exact TS boolean).
        assert_eq!(RateLimitInfo::from_headers(&[]).fallback_available, None);
        // `=== 'available'` (claudeAiLimits.ts:384-385) → Some(true).
        let avail = hdrs(&[("anthropic-ratelimit-unified-fallback", "available")]);
        assert_eq!(
            RateLimitInfo::from_headers(&avail).fallback_available,
            Some(true)
        );
        // Present but any other value → strict-equality false → Some(false).
        for other in ["unavailable", "", "AVAILABLE", " available "] {
            let h = hdrs(&[("anthropic-ratelimit-unified-fallback", other)]);
            assert_eq!(
                RateLimitInfo::from_headers(&h).fallback_available,
                Some(false),
                "value {other:?} must be Some(false)"
            );
        }
    }
}

#[cfg(test)]
mod raw_utilization {
    //! Tests for [`RawUtilization::from_headers`], pinned against claude-code
    //! `extractRawUtilization` (`claudeAiLimits.ts:164-179`): a window needs
    //! BOTH its `-utilization` AND `-reset` headers (ts:174) — emitted
    //! atomically (both fields) or not at all.
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn raw_utilization_requires_both_headers_per_window() {
        // 5h has BOTH headers → Some; 7d has only -utilization → None
        // (`util !== null && reset !== null`, claudeAiLimits.ts:174).
        let headers = h(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1750000005"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.77"),
        ]);
        let raw = RawUtilization::from_headers(&headers);
        assert_eq!(
            raw.five_hour,
            Some(RawWindow {
                utilization: 0.42,
                resets_at: 1_750_000_005,
            })
        );
        assert_eq!(raw.seven_day, None);
    }

    #[test]
    fn raw_utilization_empty_headers_is_default() {
        let raw = RawUtilization::from_headers(&[]);
        assert_eq!(raw, RawUtilization::default());
        assert_eq!(raw.five_hour, None);
        assert_eq!(raw.seven_day, None);
    }

    #[test]
    fn raw_utilization_malformed_values_drop_window() {
        // Malformed 5h utilization drops ONLY that window (the file's
        // tolerant-parse stance; TS would store NaN — we fail soft to None);
        // the well-formed 7d window still parses.
        let headers = h(&[
            ("anthropic-ratelimit-unified-5h-utilization", "garbage"),
            ("anthropic-ratelimit-unified-5h-reset", "1750000005"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.77"),
            ("anthropic-ratelimit-unified-7d-reset", "1750000007"),
        ]);
        let raw = RawUtilization::from_headers(&headers);
        assert_eq!(raw.five_hour, None);
        assert_eq!(
            raw.seven_day,
            Some(RawWindow {
                utilization: 0.77,
                resets_at: 1_750_000_007,
            })
        );

        // Malformed RESET also drops the window — atomic either-both-or-none.
        let headers = h(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "soon"),
        ]);
        let raw = RawUtilization::from_headers(&headers);
        assert_eq!(raw.five_hour, None);
        assert_eq!(raw.seven_day, None);
        assert_eq!(raw, RawUtilization::default());
    }
}

#[cfg(test)]
mod early_warning {
    //! Early-warning port tests, pinned against claude-code
    //! `getHeaderBasedEarlyWarning` (`claudeAiLimits.ts:255-294`),
    //! `getTimeRelativeEarlyWarning` (`:301-340`),
    //! `getEarlyWarningFromHeaders` (`:347-374`) and the
    //! `computeNewLimitsFromHeaders` final-status semantics (`:411-424`).
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Fixed clock: `UNIX_EPOCH + secs` (the TS code reads `Date.now()/1000`).
    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn surpassed_threshold_header_forces_allowed_warning_replacement() {
        // getHeaderBasedEarlyWarning (claudeAiLimits.ts:255-294): the
        // surpassed-threshold header replaces the regular parse with a FRESH
        // allowed_warning object built from the per-claim headers.
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-fallback", "available"),
            ("anthropic-ratelimit-unified-7d-surpassed-threshold", "0.5"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.55"),
            ("anthropic-ratelimit-unified-7d-reset", "1750000000"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("seven_day"));
        assert_eq!(p.utilization, Some(0.55));
        assert_eq!(p.resets_at, Some(1_750_000_000));
        assert_eq!(p.surpassed_threshold, Some(0.5));
        assert_eq!(p.fallback_available, Some(true));
        // Fresh-object semantics (claudeAiLimits.ts:281-289): the replacement
        // carries NO overage fields even though the header was present.
        assert_eq!(p.overage_status, None);
    }

    #[test]
    fn claim_priority_is_5h_then_7d_then_overage() {
        // EARLY_WARNING_CLAIM_MAP iteration order (claudeAiLimits.ts:73-77,
        // :260-262): '5h' is checked first, so it wins over '7d'.
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-surpassed-threshold", "0.9"),
            ("anthropic-ratelimit-unified-7d-surpassed-threshold", "0.5"),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(p.surpassed_threshold, Some(0.9));
    }

    #[test]
    fn overage_claim_surpassed_threshold_maps_to_overage_type() {
        // 'overage' → 'overage' (claudeAiLimits.ts:76).
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            (
                "anthropic-ratelimit-unified-overage-surpassed-threshold",
                "0.8",
            ),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("overage"));
        assert_eq!(p.surpassed_threshold, Some(0.8));
    }

    #[test]
    fn surpassed_threshold_header_fires_on_presence_even_when_malformed() {
        // getHeaderBasedEarlyWarning gates on header PRESENCE alone
        // (`!== null`, claudeAiLimits.ts:268) — the value is only `Number()`ed
        // for storage (ts:288). A malformed value therefore still fires the
        // warning; our documented divergence stores `None` where TS would
        // store `NaN`.
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            (
                "anthropic-ratelimit-unified-5h-surpassed-threshold",
                "garbage",
            ),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(p.surpassed_threshold, None);
    }

    #[test]
    fn time_relative_5h_fires_at_high_utilization_early_in_window() {
        // getTimeRelativeEarlyWarning, five_hour config (claudeAiLimits.ts:54-59):
        // threshold {utilization: 0.9, timePct: 0.72}, window 18000s.
        // elapsed = 1_000_000 − (1_009_000 − 18_000) = 9_000 → progress 0.5 ≤ 0.72
        // and 0.95 ≥ 0.9 → warn.
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.95"),
            ("anthropic-ratelimit-unified-5h-reset", "1009000"),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(p.utilization, Some(0.95));
        assert_eq!(p.resets_at, Some(1_009_000));
        // The time-relative fresh object has NO surpassedThreshold
        // (claudeAiLimits.ts:332-339).
        assert_eq!(p.surpassed_threshold, None);
    }

    #[test]
    fn time_relative_5h_does_not_fire_late_in_window() {
        // elapsed = 1_000_000 − (1_001_000 − 18_000) = 17_000 → progress
        // ≈ 0.944 > 0.72 → no warn. The bare allowed_warning status is then
        // DOWNGRADED to allowed (claudeAiLimits.ts:423 `finalStatus = 'allowed'`).
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.95"),
            ("anthropic-ratelimit-unified-5h-reset", "1001000"),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed"));
        assert_eq!(p.surpassed_threshold, None);
    }

    #[test]
    fn time_relative_boundaries_are_inclusive() {
        // Both comparisons are INCLUSIVE (`utilization >= t.utilization &&
        // timeProgress <= t.timePct`, claudeAiLimits.ts:324-326): utilization
        // EXACTLY 0.9 and timeProgress EXACTLY 0.72 still warn on the
        // five_hour config. Exact-representable arithmetic: window 18_000,
        // elapsed = 1_000_000 − (1_005_040 − 18_000) = 12_960 →
        // 12_960 / 18_000 == 0.72 exactly (correctly rounded quotient equals
        // the 0.72 literal).
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
            ("anthropic-ratelimit-unified-5h-reset", "1005040"),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(p.utilization, Some(0.9));
        assert_eq!(p.resets_at, Some(1_005_040));
        assert_eq!(p.surpassed_threshold, None);
    }

    #[test]
    fn time_relative_7d_middle_threshold() {
        // seven_day config (claudeAiLimits.ts:60-69), middle threshold
        // {utilization: 0.5, timePct: 0.35}: window 604_800s,
        // reset = 2_000_000 + (604_800 − 181_440) → elapsed 181_440 →
        // progress 0.3 ≤ 0.35 and 0.6 ≥ 0.5 → warn (utilization 0.6 < 0.75
        // keeps the first threshold from firing — `.some()` over all).
        let reset = 2_000_000 + (604_800 - 181_440);
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.6"),
            ("anthropic-ratelimit-unified-7d-reset", &reset.to_string()),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(2_000_000));
        assert_eq!(p.status.as_deref(), Some("allowed_warning"));
        assert_eq!(p.rate_limit_type.as_deref(), Some("seven_day"));
        assert_eq!(p.utilization, Some(0.6));
    }

    #[test]
    fn rejected_status_passes_through_untouched_by_early_warning() {
        // computeNewLimitsFromHeaders only consults the early warning when
        // status is allowed/allowed_warning (claudeAiLimits.ts:414); 'rejected'
        // falls through to the regular parse, overage fields intact.
        let headers = h(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-surpassed-threshold", "0.9"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
        ]);
        let p = RateLimitInfo::from_headers_at(&headers, at(1_000_000));
        assert_eq!(p.status.as_deref(), Some("rejected"));
        assert_eq!(p.overage_status.as_deref(), Some("allowed"));
        assert_eq!(p.surpassed_threshold, None);
    }

    #[test]
    fn missing_status_header_stays_none_no_fabricated_allowed() {
        // Deliberate divergence-guard: TS defaults a missing status to
        // 'allowed' (claudeAiLimits.ts:379-381), but fabricating a status from
        // zero unified headers would break `has_unified_headers()` consumers.
        let p = RateLimitInfo::from_headers_at(&[], at(1_000_000));
        assert_eq!(p.status, None);
        assert_eq!(p, RateLimitInfo::default());
    }

    #[test]
    fn from_headers_delegates_with_wall_clock() {
        // The public wrapper still exists and parses with `SystemTime::now()`.
        let headers = h(&[("anthropic-ratelimit-unified-status", "rejected")]);
        let p = RateLimitInfo::from_headers(&headers);
        assert_eq!(p.status.as_deref(), Some("rejected"));
    }
}

/// Task 6 (llm-runtime future-work batch 5): the fresh-from-error-headers
/// rejected-limits constructor (`errors.ts:471-516`).
#[cfg(test)]
mod from_429_error_headers {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Gate (`errors.ts:480`): no representative-claim AND no overage-status
    /// → `None` (claude-code falls through to the generic 429 branch).
    #[test]
    fn gate_fails_without_unified_headers() {
        assert_eq!(RateLimitInfo::from_429_error_headers(&[]), None);
        // Other unified headers alone don't pass the gate.
        let headers = h(&[
            ("anthropic-ratelimit-unified-reset", "1760000000"),
            ("anthropic-ratelimit-unified-status", "rejected"),
        ]);
        assert_eq!(RateLimitInfo::from_429_error_headers(&headers), None);
        // Empty values are falsy in the TS gate.
        let empty = h(&[
            ("anthropic-ratelimit-unified-representative-claim", ""),
            ("anthropic-ratelimit-unified-overage-status", ""),
        ]);
        assert_eq!(RateLimitInfo::from_429_error_headers(&empty), None);
    }

    /// Full header set maps the five TS-set fields; status is FORCED
    /// 'rejected' (errors.ts:483) even when the status header says otherwise.
    #[test]
    fn full_headers_map_with_forced_rejected_status() {
        let headers = h(&[
            (
                "anthropic-ratelimit-unified-representative-claim",
                "seven_day",
            ),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-reset", "1760000000"),
            ("anthropic-ratelimit-unified-overage-reset", "1760000200"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "out_of_credits",
            ),
        ]);
        let info = RateLimitInfo::from_429_error_headers(&headers).expect("gate passes");
        assert_eq!(
            info.status.as_deref(),
            Some("rejected"),
            "forced (errors.ts:483)"
        );
        assert_eq!(info.rate_limit_type.as_deref(), Some("seven_day"));
        assert_eq!(info.overage_status.as_deref(), Some("rejected"));
        assert_eq!(info.resets_at, Some(1_760_000_000));
        assert_eq!(info.overage_resets_at, Some(1_760_000_200));
        assert_eq!(
            info.overage_disabled_reason.as_deref(),
            Some("out_of_credits")
        );
        // Fields the TS error path never sets stay at their defaults.
        assert_eq!(info.utilization, None);
        assert_eq!(info.claim_resets_at, None);
        assert_eq!(info.fallback_available, None);
        assert_eq!(info.surpassed_threshold, None);
    }

    /// representative-claim alone passes the gate; NO early-warning
    /// replacement runs (unlike `from_headers`, the error path builds the
    /// object directly — a surpassed-threshold header is ignored).
    #[test]
    fn claim_only_passes_gate_and_skips_early_warning() {
        let headers = h(&[
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-5h-surpassed-threshold", "0.9"),
        ]);
        let info = RateLimitInfo::from_429_error_headers(&headers).expect("gate passes");
        assert_eq!(info.status.as_deref(), Some("rejected"));
        assert_eq!(info.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(
            info.surpassed_threshold, None,
            "no early-warning on the error path"
        );
    }

    /// The constructed info composes the byte-locked rejected copy through
    /// `rate_limit_error_message` (rateLimitMessages.ts:333-344).
    #[test]
    fn composes_byte_locked_rejected_copy() {
        let headers = h(&[(
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day",
        )]);
        let info = RateLimitInfo::from_429_error_headers(&headers).expect("gate passes");
        let msg = rate_limit_error_message(
            &info,
            &ResetTimes {
                reset_time: Some("3pm"),
                ..ResetTimes::default()
            },
            SubscriptionContext::default(),
        );
        assert_eq!(
            msg.as_deref(),
            Some("You've hit your weekly limit · resets 3pm")
        );
    }
}

/// Task 5: `credits_required` error-BODY derivation — claude-code `Nqi(e)`
/// (`if error.error.details.error_code==="credits_required"` →
/// `overageDisabledReason = details.disabled_reason`).
#[cfg(test)]
mod from_429_error_body {
    use super::*;

    /// The exact body shape from the task brief: no unified headers at all,
    /// just the credits-required error body. The body alone must pass the
    /// gate (claude-code derives `Nqi` from a separate error-catch site than
    /// the header-only `errors.ts:471-516` object) and populate both
    /// `credits_required` and the body-derived `overage_disabled_reason`.
    #[test]
    fn credits_required_body_sets_flag_and_disabled_reason() {
        let body = serde_json::json!({
            "error": {
                "error": {
                    "details": {
                        "error_code": "credits_required",
                        "disabled_reason": "out_of_credits"
                    }
                }
            }
        });
        let info = RateLimitInfo::from_429_error(&[], Some(&body)).expect("body alone passes gate");
        assert!(info.credits_required, "Nqi sets credits_required=true");
        assert_eq!(
            info.overage_disabled_reason.as_deref(),
            Some("out_of_credits")
        );
        assert_eq!(
            info.status.as_deref(),
            Some("rejected"),
            "forced rejected status, same as the header path"
        );
    }

    /// No body at all → identical to `from_429_error_headers` (delegation
    /// preserves every existing header-only behaviour).
    #[test]
    fn no_body_behaves_like_header_only() {
        assert_eq!(RateLimitInfo::from_429_error(&[], None), None);
        let headers: Vec<(String, String)> = vec![(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "seven_day".to_string(),
        )];
        let with_body = RateLimitInfo::from_429_error(&headers, None);
        let via_headers_fn = RateLimitInfo::from_429_error_headers(&headers);
        assert_eq!(with_body, via_headers_fn);
    }

    /// A body with a non-`credits_required` `error_code` (or no `details` at
    /// all) never sets the flag and never passes the gate on its own.
    #[test]
    fn unrelated_error_code_does_not_set_flag() {
        let body = serde_json::json!({
            "error": {"error": {"details": {"error_code": "something_else"}}}
        });
        assert_eq!(RateLimitInfo::from_429_error(&[], Some(&body)), None);

        let no_details = serde_json::json!({"error": {"error": {}}});
        assert_eq!(RateLimitInfo::from_429_error(&[], Some(&no_details)), None);
    }

    /// When BOTH the header and the body carry a disabled reason, the header
    /// wins (documented precedence — the header-driven parse runs first and
    /// only falls back to the body's reason when absent).
    #[test]
    fn header_disabled_reason_takes_precedence_over_body() {
        let headers: Vec<(String, String)> = vec![
            (
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "seven_day".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason".to_string(),
                "header_reason".to_string(),
            ),
        ];
        let body = serde_json::json!({
            "error": {
                "error": {
                    "details": {
                        "error_code": "credits_required",
                        "disabled_reason": "body_reason"
                    }
                }
            }
        });
        let info = RateLimitInfo::from_429_error(&headers, Some(&body)).expect("gate passes");
        assert!(info.credits_required);
        assert_eq!(
            info.overage_disabled_reason.as_deref(),
            Some("header_reason")
        );
    }

    /// `RateLimitInfo::default()` (the success-path baseline, and any struct
    /// built without going through the 429-error constructors) leaves
    /// `credits_required` at its default `false`.
    #[test]
    fn credits_required_defaults_false() {
        assert!(!RateLimitInfo::default().credits_required);
    }
}
