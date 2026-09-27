//! Rate-limit header parsing for Anthropic responses.
//!
//! Spec §7 (lines 665-666):
//! * `Retry-After` — seconds, RFC 7231 §7.1.3 delta-seconds form.
//! * `anthropic-ratelimit-requests-reset` — ISO8601 UTC.
//!
//! The user-facing error string `"Rate limited; retrying in {N}s"` is locked
//! byte-for-byte (spec §5 recovery-strategy table line 517).

#![forbid(unsafe_code)]

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Locked rate-limit error message format. Used by `ApiError::RateLimited::fmt`.
/// Spec §5 line 517 — byte-for-byte against claude-code.
pub const RATE_LIMIT_ERR_FMT: &str = "Rate limited; retrying in {N}s";

/// Build the byte-locked user-facing rate-limit error string for `secs` seconds.
#[must_use]
pub fn format_rate_limited_msg(secs: u64) -> String {
    format!("Rate limited; retrying in {secs}s")
}

/// Look up `header_name` case-insensitively in a header vec and return the
/// first matching value, if any.
pub(crate) fn header_value<'a>(
    headers: &'a [(String, String)],
    header_name: &str,
) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(header_name))
        .map(|(_, v)| v.as_str())
}

/// Parse the `Retry-After` header (RFC 7231 §7.1.3 delta-seconds form).
///
/// Returns `None` if the header is missing, empty, or in the HTTP-date form
/// (which we deliberately don't support — claude-code parity).
#[must_use]
pub fn parse_retry_after(headers: &[(String, String)]) -> Option<Duration> {
    let raw = header_value(headers, "retry-after")?.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse::<u64>().ok().map(Duration::from_secs)
}

/// Parse a Go-duration string (e.g. `"6m0s"`, `"1.5s"`, `"880ms"`,
/// `"2m59.56s"`, `"1h2m3s"`) into a [`Duration`]. A bare number with no unit is
/// treated as seconds. Returns `None` on a malformed value.
#[must_use]
pub fn parse_go_duration(raw: &str) -> Option<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut idx = 0;
    let mut total_secs = 0f64;
    let mut saw_component = false;
    while idx < bytes.len() {
        let num_start = idx;
        while idx < bytes.len() && (bytes[idx].is_ascii_digit() || bytes[idx] == b'.') {
            idx += 1;
        }
        if idx == num_start {
            return None; // a unit with no preceding number
        }
        let num: f64 = s[num_start..idx].parse().ok()?;
        let unit_start = idx;
        while idx < bytes.len() && !bytes[idx].is_ascii_digit() && bytes[idx] != b'.' {
            idx += 1;
        }
        let mult = match &s[unit_start..idx] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            "us" | "µs" => 0.000_001,
            "ns" => 0.000_000_001,
            "" => 1.0, // bare number → seconds
            _ => return None,
        };
        total_secs += num * mult;
        saw_component = true;
    }
    if !saw_component || !total_secs.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(total_secs.max(0.0)))
}

/// Resolve an OpenAI-style 429 reset delay from the `x-ratelimit-reset-requests`
/// / `x-ratelimit-reset-tokens` headers (Go-duration values).
///
/// OpenAI returns the time until each limited bucket refills here rather than as
/// a plain `Retry-After`, so without parsing these the retry driver falls back
/// to a 1s blind wait and hammers the still-exhausted window. When both are
/// present we wait the LONGER of the two (the binding bucket) to avoid retrying
/// into a window that hasn't refilled.
#[must_use]
pub fn parse_openai_reset(headers: &[(String, String)]) -> Option<Duration> {
    let one = |name: &str| header_value(headers, name).and_then(parse_go_duration);
    match (
        one("x-ratelimit-reset-requests"),
        one("x-ratelimit-reset-tokens"),
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Parse `anthropic-ratelimit-requests-reset` (ISO8601 `YYYY-MM-DDTHH:MM:SSZ`)
/// into a `Duration` relative to `now`. Negative diffs (past timestamps)
/// clamp to `Duration::ZERO`. Non-conforming values return `None`.
///
/// The parser is hand-rolled to avoid pulling `chrono` into the runtime
/// crate; it accepts the exact `YYYY-MM-DDTHH:MM:SSZ` shape claude-code emits.
#[must_use]
pub fn parse_anthropic_ratelimit_reset(
    headers: &[(String, String)],
    now: SystemTime,
) -> Option<Duration> {
    let raw = header_value(headers, "anthropic-ratelimit-requests-reset")?.trim();
    let target = parse_iso8601_utc(raw)?;
    let now_secs = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(target.saturating_sub(now_secs)))
}

/// Cap a persistent rate-limit reset wait at 6 hours (claude-code
/// `PERSISTENT_RESET_CAP_MS`).
pub const PERSISTENT_RESET_CAP_MS: u64 = 6 * 60 * 60 * 1000;

/// Parse `anthropic-ratelimit-unified-reset` (a Unix-epoch **seconds** value —
/// distinct from the ISO8601 `…-requests-reset` above) into a `Duration` from
/// `now`, capped at [`PERSISTENT_RESET_CAP_MS`]. 1:1 with claude-code
/// `getRateLimitResetDelayMs` (`withRetry.ts:814-821`): a past-or-equal reset
/// (`delayMs <= 0`) returns `None` so the caller falls through to the next
/// delay source (NOT clamped to zero, unlike the ISO parser above).
///
/// Divergence: claude-code uses JS `Number()`, accepting decimal/scientific
/// forms; we parse integer epoch seconds (the form the server sends). A
/// non-integer value yields `None` (same fail-soft outcome).
#[must_use]
pub fn parse_unified_reset(headers: &[(String, String)], now: SystemTime) -> Option<Duration> {
    let raw = header_value(headers, "anthropic-ratelimit-unified-reset")?.trim();
    let reset_unix_sec: u64 = raw.parse().ok()?;
    let now_ms = u64::try_from(now.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()?;
    let reset_ms = reset_unix_sec.checked_mul(1000)?;
    // checked_sub → None for a PAST reset; an exactly-now reset (0) also yields
    // None (claude-code `delayMs <= 0 → null`).
    let delay_ms = reset_ms.checked_sub(now_ms)?;
    if delay_ms == 0 {
        return None;
    }
    Some(Duration::from_millis(delay_ms.min(PERSISTENT_RESET_CAP_MS)))
}

/// 24h in milliseconds — the `formatResetTime` date-vs-time branch boundary
/// (TS `hoursUntilReset > 24`).
const TWENTY_FOUR_HOURS_MS: i64 = 24 * 60 * 60 * 1000;

/// Port of claude-code `formatResetTime` (`utils/format.ts:238-289`).
///
/// Renders a rate-limit reset timestamp (Unix **seconds**) the way claude-code
/// does for the 429 message: `en-US` locale, 12-hour clock, the minute omitted
/// when it is `:00`, the space before AM/PM removed, and AM/PM lowercased — e.g.
/// `"3pm"`, `"3:30pm"`, `"Jun 7, 3:30pm"`. Resets more than 24h out include the
/// month/day (and the year when it differs from the current year); resets within
/// 24h render the time only. `show_time = false` drops the time entirely on the
/// far-future branch. Returns `None` for a `None`/zero timestamp (TS
/// `if (!timestampInSeconds) return undefined`).
///
/// All component reads (minute, year, formatting) use **local** time, matching
/// the TS `Date` accessors (`getMinutes`/`getFullYear`) and `toLocaleString`.
///
/// `show_timezone` appends `" (<tz>)"`. See [`reset_time_zone`] for the
/// documented divergence from TS `getTimeZone()` (IANA name vs. offset).
#[must_use]
pub fn format_reset_time(
    timestamp_secs: Option<i64>,
    show_timezone: bool,
    show_time: bool,
) -> Option<String> {
    format_reset_time_at(timestamp_secs, Local::now(), show_timezone, show_time)
}

/// Testable core of [`format_reset_time`] with an injected `now` (so unit tests
/// never read the wall clock). `now` is a `DateTime<Local>`; the reset timestamp
/// is interpreted in the same local zone, exactly like the TS `Date` accessors.
#[must_use]
fn format_reset_time_at(
    timestamp_secs: Option<i64>,
    now: DateTime<Local>,
    show_timezone: bool,
    show_time: bool,
) -> Option<String> {
    // TS: `if (!timestampInSeconds) return undefined` — 0 is falsy too.
    let ts = match timestamp_secs {
        Some(t) if t != 0 => t,
        _ => return None,
    };

    // `new Date(timestampInSeconds * 1000)` in the local zone. A timestamp that
    // can't be represented (out of range) fails soft to `None`.
    let date = Local.timestamp_opt(ts, 0).single()?;

    let minutes = date.minute();

    // TS: `hoursUntilReset = (date - now) / 3_600_000ms`, branch on `> 24`.
    // Equivalent integer test `(date_ms - now_ms) > 24h_in_ms` — exact, no float
    // cast, same `>` boundary (a reset exactly 24h out stays time-only).
    let is_far_future = date.timestamp_millis() - now.timestamp_millis() > TWENTY_FOUR_HOURS_MS;

    let tz_suffix = |s: String| -> String {
        if show_timezone {
            format!("{s} ({})", reset_time_zone(date))
        } else {
            s
        }
    };

    if is_far_future {
        // Far-future branch: `month: short, day: numeric` always; `hour`/minute
        // only when `show_time`; `year` only when it differs from `now`.
        // en-US layouts (verified against Node `toLocaleString`):
        //   "Jun 7"                 (show_time = false)
        //   "Jun 7, 3pm"            (minutes == 0)
        //   "Jun 7, 3:30pm"         (minutes != 0)
        //   "Jun 7, 2027, 3:30pm"   (year differs)
        let mut out = format!("{} {}", date.format("%b"), date.day());
        if date.year() != now.year() {
            out.push_str(&format!(", {}", date.year()));
        }
        if show_time {
            out.push_str(&format!(", {}", format_en_us_time(&date, minutes)));
        }
        return Some(tz_suffix(out));
    }

    // Within 24h: time only, e.g. "3pm" / "3:30pm".
    Some(tz_suffix(format_en_us_time(&date, minutes)))
}

/// Format the `en-US` 12-hour time component with the claude-code lowercasing
/// applied: hour without a leading zero, the minute as `:MM` only when non-zero,
/// then the AM/PM marker lowercased and joined with no space. Reproduces TS
/// `toLocaleTimeString('en-US', { hour:'numeric', minute: …, hour12:true })`
/// followed by `.replace(/ ([AP]M)/i, …toLowerCase())`.
fn format_en_us_time(date: &DateTime<Local>, minutes: u32) -> String {
    // `%-I` = 12-hour, no leading zero; `%p` = `AM`/`PM`.
    let hour = date.format("%-I");
    let ampm = date.format("%p").to_string().to_lowercase();
    if minutes == 0 {
        format!("{hour}{ampm}")
    } else {
        format!("{hour}:{minutes:02}{ampm}")
    }
}

/// Timezone string appended when `show_timezone` is set.
///
/// Byte-faithful to claude-code `getTimeZone()`
/// (`Intl.DateTimeFormat().resolvedOptions().timeZone`), which returns the IANA
/// zone *name* (e.g. `"America/Los_Angeles"`). `iana-time-zone` is already in the
/// locked dependency graph (transitive via chrono), so this adds an edge, not a
/// new package. If the host zone cannot be resolved (rare), we fall back to the
/// chrono `%Z` local-offset rendering (e.g. `"+08:00"`) so the suffix is never
/// empty — `date` is retained only for that fallback.
fn reset_time_zone(date: DateTime<Local>) -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| date.format("%Z").to_string())
}

/// Owned, already-formatted reset strings derived from the unified-reset
/// response headers. Lives one frame above [`ResetTimes`] (which borrows) so the
/// caller can hand the borrowed view to [`rate_limit_error_message`].
///
/// 1:1 with the header reads in claude-code `computeNewLimitsFromHeaders`
/// (`claudeAiLimits.ts:382-398`) feeding `getLimitReachedText`
/// (`rateLimitMessages.ts:143-166`): `resetsAt =
/// Number(anthropic-ratelimit-unified-reset)`, `overageResetsAt =
/// Number(anthropic-ratelimit-unified-overage-reset)`, both rendered with
/// `formatResetTime(..., /* showTimezone */ true)`, and the dual-window branch
/// picking the earlier of `resetsAt < overageResetsAt`.
#[derive(Debug, Clone, Default)]
pub struct FormattedResetTimes {
    /// `formatResetTime(resetsAt, true)`, if `resetsAt` was present.
    pub reset_time: Option<String>,
    /// `formatResetTime(overageResetsAt, true)`, if `overageResetsAt` present.
    pub overage_reset_time: Option<String>,
    /// `resetsAt < overageResetsAt` when both raw timestamps were present.
    pub reset_is_earlier: Option<bool>,
}

impl FormattedResetTimes {
    /// Borrowed view consumed by [`rate_limit_error_message`].
    #[must_use]
    pub fn as_reset_times(&self) -> ResetTimes<'_> {
        ResetTimes {
            reset_time: self.reset_time.as_deref(),
            overage_reset_time: self.overage_reset_time.as_deref(),
            reset_is_earlier: self.reset_is_earlier,
        }
    }
}

/// Read the unified reset headers and produce the locale-formatted reset strings
/// the 429 message consumes — the api-client analogue of claude-code mapping the
/// reset headers through `formatResetTime`. `showTimezone = true` matches the TS
/// call sites (`rateLimitMessages.ts:145-148`).
#[must_use]
pub fn formatted_reset_times_from_headers(headers: &[(String, String)]) -> FormattedResetTimes {
    formatted_reset_times_at(headers, Local::now())
}

/// Testable core of [`formatted_reset_times_from_headers`] with an injected
/// `now` so unit tests never read the wall clock.
#[must_use]
fn formatted_reset_times_at(
    headers: &[(String, String)],
    now: DateTime<Local>,
) -> FormattedResetTimes {
    // `Number(header)` in TS: an absent/blank/non-numeric value yields `None`
    // (NaN → falsy → `resetsAt` undefined), so the reset clause is dropped.
    let parse = |name: &str| -> Option<i64> {
        header_value(headers, name)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i64>().ok())
    };
    let resets_at = parse("anthropic-ratelimit-unified-reset");
    let overage_resets_at = parse("anthropic-ratelimit-unified-overage-reset");

    FormattedResetTimes {
        reset_time: format_reset_time_at(resets_at, now, true, true),
        overage_reset_time: format_reset_time_at(overage_resets_at, now, true, true),
        // `resetsAt < limits.overageResetsAt` (rateLimitMessages.ts:160) — only
        // meaningful when both raw timestamps are present.
        reset_is_earlier: match (resets_at, overage_resets_at) {
            (Some(r), Some(o)) => Some(r < o),
            _ => None,
        },
    }
}

/// The `anthropic-ratelimit-unified-overage-disabled-reason` header value, if
/// the server signalled that overage spend is disabled (claude-code
/// `withRetry.ts:276`). Surfaced so callers can avoid retrying a 429 that
/// cannot succeed until the window resets.
#[must_use]
pub fn overage_disabled_reason(headers: &[(String, String)]) -> Option<&str> {
    header_value(
        headers,
        "anthropic-ratelimit-unified-overage-disabled-reason",
    )
}

/// Parsed unified rate-limit state used to render the user-facing 429 message.
///
/// Mirrors the subset of claude-code `ClaudeAILimits` (claudeAiLimits.ts:122)
/// that the *error* (rejected) message path reads: the representative claim
/// (`rate_limit_type`), the overage status, and the overage-disabled reason.
/// The reset-time strings are pre-formatted by the caller (claude-code threads
/// `formatResetTime(...)` output, which is locale/timezone dependent and thus
/// not byte-reproducible here) — the *templates* around them are byte-locked.
// `Eq` cannot be derived since `utilization: Option<f64>` (NaN never occurs —
// parsing filters non-finite values — but the type still forbids `Eq`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimitInfo {
    /// `anthropic-ratelimit-unified-representative-claim` — which window was
    /// exhausted (`five_hour` / `seven_day` / `seven_day_opus` /
    /// `seven_day_sonnet`). `None` when the header is absent.
    pub rate_limit_type: Option<String>,
    /// `anthropic-ratelimit-unified-overage-status` — `allowed` /
    /// `allowed_warning` / `rejected`. `None` when the header is absent.
    pub overage_status: Option<String>,
    /// `anthropic-ratelimit-unified-overage-disabled-reason` — e.g.
    /// `out_of_credits`. `None` when the header is absent.
    pub overage_disabled_reason: Option<String>,
    /// `anthropic-ratelimit-unified-status` — `allowed` / `allowed_warning` /
    /// `rejected`. `None` when the header is absent or empty; claude-code
    /// defaults that case to `'allowed'` via `headers.get(…) || 'allowed'`
    /// (`claudeAiLimits.ts:379-381`), so `status.as_deref().unwrap_or("allowed")`
    /// reproduces the TS value exactly.
    pub status: Option<String>,
    /// `anthropic-ratelimit-unified-reset` — Unix-epoch **seconds** when the
    /// representative window resets (`claudeAiLimits.ts:382-383`,
    /// `Number(resetsAtHeader)`). Absent/malformed → `None` (TS would store
    /// `NaN` for a malformed value; we fail soft, same as
    /// [`parse_unified_reset`]).
    pub resets_at: Option<u64>,
    /// Per-claim `anthropic-ratelimit-unified-{abbrev}-utilization` (0-1
    /// fraction) for the representative claim's abbrev — see [`claim_abbrev`]
    /// and `extractRawUtilization` (`claudeAiLimits.ts:164-179`). `None` when
    /// the claim has no per-claim headers (`seven_day_opus` /
    /// `seven_day_sonnet` / unknown), the header is absent, or the value is
    /// malformed/non-finite.
    pub utilization: Option<f64>,
    /// Per-claim `anthropic-ratelimit-unified-{abbrev}-reset` — Unix-epoch
    /// seconds when the representative claim's window resets
    /// (`claudeAiLimits.ts:173,272-279`). Same `None` conditions as
    /// [`Self::utilization`].
    pub claim_resets_at: Option<u64>,
    /// `anthropic-ratelimit-unified-overage-reset` — Unix-epoch seconds when
    /// the overage window resets (`claudeAiLimits.ts:394-399`).
    /// Absent/malformed → `None`.
    pub overage_resets_at: Option<u64>,
    /// `anthropic-ratelimit-unified-fallback` strict-equals `"available"`
    /// (`claudeAiLimits.ts:384-385`, no trim/case-fold). `None` when the
    /// header is absent (TS collapses that to `false`;
    /// `fallback_available.unwrap_or(false)` reproduces the exact TS boolean);
    /// `Some(false)` when present with any other value.
    pub fallback_available: Option<bool>,
    /// `surpassedThreshold` (`claudeAiLimits.ts:135`) — the warning-threshold
    /// fraction from `anthropic-ratelimit-unified-{abbrev}-surpassed-threshold`.
    /// Set ONLY by the header-based early-warning replacement
    /// (`claudeAiLimits.ts:288`); the base header parse and the time-relative
    /// fallback leave it `None` (the TS fresh object at `:332-339` omits it).
    pub surpassed_threshold: Option<f64>,
    /// `anthropic-ratelimit-unified-overage-in-use` strict-equals `"true"`
    /// (2.1.206 `ClaudeAILimits`: `overageInUse = header(…) === 'true'`). No
    /// header → `false` (the TS boolean has no `undefined` state).
    pub overage_in_use: bool,
    /// `anthropic-ratelimit-unified-upgrade-paths` — a comma-separated list of
    /// upgrade-plan identifiers, split and trimmed (2.1.206:
    /// `d = header(…); d ? d.split(',').map(trim) : undefined`). `None` when
    /// the header is absent OR empty — an empty string is falsy in TS, so a
    /// present-but-empty header collapses to `None`, same as `status`.
    pub upgrade_paths: Option<Vec<String>>,
    /// `anthropic-ratelimit-unified-overage-period-monthly-utilization` (0-1
    /// fraction; 2.1.206: `Number(header(…))` when finite). `None` when the
    /// header is absent or non-finite.
    pub overage_period_monthly_utilization: Option<f64>,
    /// `anthropic-ratelimit-unified-overage-period-channel-utilization` (0-1
    /// fraction; 2.1.206: `Number(header(…))` when finite). `None` when the
    /// header is absent or non-finite.
    pub overage_period_channel_utilization: Option<f64>,
    /// `true` when the 429 error BODY's `error.error.details.error_code`
    /// equals `"credits_required"` — the crate's analogue of claude-code
    /// `Nqi(e)`, which reads the raw error body (not the response headers).
    /// Set only by [`Self::from_429_error`] when a body is supplied; defaults
    /// `false` (mirrors the TS falsy/absent case). Feeds the composer's
    /// `jid` upsell-suppression gate:
    /// `rateLimitType==="seven_day_overage_included" || credits_required`.
    pub credits_required: bool,
}

/// Map a representative-claim value to the abbreviation used in the per-claim
/// headers `anthropic-ratelimit-unified-{abbrev}-utilization` / `-{abbrev}-reset`.
///
/// Pinned 1:1 to the only claim↔abbrev associations in claude-code:
/// `extractRawUtilization` pairs `['five_hour','5h']` / `['seven_day','7d']`
/// (`claudeAiLimits.ts:166-169`) and `EARLY_WARNING_CLAIM_MAP`
/// `{'5h':'five_hour','7d':'seven_day','overage':'overage'}`
/// (`claudeAiLimits.ts:73-77`). `seven_day_opus` / `seven_day_sonnet` have NO
/// per-claim headers anywhere in the TS — the full unified-header inventory
/// (`mockRateLimits.ts:33-40`) lists only `5h` / `7d` / `overage` variants —
/// so they, like unknown claims, return `None` (TS never attaches a
/// utilization to those claims).
fn claim_abbrev(claim: &str) -> Option<&'static str> {
    match claim {
        "five_hour" => Some("5h"),
        "seven_day" => Some("7d"),
        "overage" => Some("overage"),
        _ => None,
    }
}

/// Tolerant epoch-seconds read, mirroring TS `Number(header)` fail-soft:
/// absent/blank/non-numeric → `None`. Divergence (same stance as
/// `parse_unified_reset`): TS `Number()` accepts decimal/scientific epoch
/// forms; we parse integer epoch seconds (the form the server sends) and fail
/// soft otherwise.
fn parse_epoch_secs(headers: &[(String, String)], name: &str) -> Option<u64> {
    header_value(headers, name)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Tolerant 0-1-fraction read (utilization / surpassed-threshold headers).
/// Non-finite values (`NaN`/`inf` parse as valid f64 in Rust) are rejected so
/// `RateLimitInfo: PartialEq` comparisons stay total in practice.
fn parse_fraction(headers: &[(String, String)], name: &str) -> Option<f64> {
    header_value(headers, name)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite())
}

/// One window of raw utilization (`RawWindowUtilization`,
/// `claudeAiLimits.ts:150-153`). Both fields are required — a window is
/// parsed ATOMICALLY (both `-utilization` and `-reset` headers, ts:174) or
/// not at all, so a `RawWindow` can never carry a dangling half.
// `Eq` cannot be derived (`utilization: f64`); `parse_fraction` filters
// non-finite values so `PartialEq` stays total in practice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawWindow {
    /// 0-1 utilization fraction.
    pub utilization: f64,
    /// Unix epoch seconds when the window resets.
    pub resets_at: u64,
}

/// Raw per-window utilization — claude-code `extractRawUtilization`
/// (`claudeAiLimits.ts:164-179`). Tracked on EVERY response with unified
/// headers, independent of the warning-gated [`RateLimitInfo`] fields
/// (which only surface the representative claim's window). A window needs
/// BOTH its `-utilization` and `-reset` headers (`util !== null && reset
/// !== null`, ts:174); the default (both `None`) is the TS empty `{}`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RawUtilization {
    /// 5-hour window (`anthropic-ratelimit-unified-5h-*`).
    pub five_hour: Option<RawWindow>,
    /// 7-day window (`anthropic-ratelimit-unified-7d-*`).
    pub seven_day: Option<RawWindow>,
}

impl RawUtilization {
    /// Parse from response headers. Absent/empty headers → `Self::default()`.
    ///
    /// Per window (`['five_hour','5h'] / ['seven_day','7d']`,
    /// `claudeAiLimits.ts:166-169`): both the `-utilization` and `-reset`
    /// header must be present AND parse (the file's tolerant-parse stance —
    /// TS `Number()` would store `NaN` for a present-but-malformed value; we
    /// drop the window instead, the same fail-soft divergence documented on
    /// [`parse_fraction`] / [`parse_epoch_secs`]).
    #[must_use]
    pub fn from_headers(headers: &[(String, String)]) -> Self {
        let window = |abbrev: &str| -> Option<RawWindow> {
            let utilization = parse_fraction(
                headers,
                &format!("anthropic-ratelimit-unified-{abbrev}-utilization"),
            )?;
            let resets_at = parse_epoch_secs(
                headers,
                &format!("anthropic-ratelimit-unified-{abbrev}-reset"),
            )?;
            Some(RawWindow {
                utilization,
                resets_at,
            })
        };
        Self {
            five_hour: window("5h"),
            seven_day: window("7d"),
        }
    }
}

/// One early-warning threshold pair (`claudeAiLimits.ts:38-41`): warn when
/// usage `>= utilization` AND the elapsed window fraction `<= time_pct`
/// (high consumption early in the window).
struct EarlyWarningThreshold {
    utilization: f64,
    time_pct: f64,
}

/// One time-relative early-warning configuration (`claudeAiLimits.ts:43-48`).
struct EarlyWarningConfig {
    rate_limit_type: &'static str,
    claim_abbrev: &'static str,
    window_seconds: u64,
    thresholds: &'static [EarlyWarningThreshold],
}

/// `EARLY_WARNING_CONFIGS` (`claudeAiLimits.ts:53-70`) — time-relative
/// fallback configs in priority order (checked first to last), used when the
/// server doesn't send a surpassed-threshold header.
const EARLY_WARNING_CONFIGS: [EarlyWarningConfig; 2] = [
    EarlyWarningConfig {
        rate_limit_type: "five_hour",
        claim_abbrev: "5h",
        window_seconds: 5 * 60 * 60,
        thresholds: &[EarlyWarningThreshold {
            utilization: 0.9,
            time_pct: 0.72,
        }],
    },
    EarlyWarningConfig {
        rate_limit_type: "seven_day",
        claim_abbrev: "7d",
        window_seconds: 7 * 24 * 60 * 60,
        thresholds: &[
            EarlyWarningThreshold {
                utilization: 0.75,
                time_pct: 0.6,
            },
            EarlyWarningThreshold {
                utilization: 0.5,
                time_pct: 0.35,
            },
            EarlyWarningThreshold {
                utilization: 0.25,
                time_pct: 0.15,
            },
        ],
    },
];

/// `EARLY_WARNING_CLAIM_MAP` (`claudeAiLimits.ts:73-77`) in the object's
/// insertion order — `Object.entries` iteration order is what gives `5h`
/// priority over `7d` over `overage` in the header-based check (`:260-262`).
const EARLY_WARNING_CLAIM_MAP: [(&str, &str); 3] = [
    ("5h", "five_hour"),
    ("7d", "seven_day"),
    ("overage", "overage"),
];

/// `computeTimeProgress` (`claudeAiLimits.ts:98-103`): fraction (0-1) of the
/// window that has elapsed — `clamp((now − (resetsAt − window)) / window, 0, 1)`.
#[allow(
    clippy::cast_precision_loss,
    reason = "realistic epoch seconds (< 2^53) and the fixed window constants are exactly representable in f64 — same number math as the TS; an adversarial u64 above 2^53 does lose precision but stays finite and the clamp(0, 1) keeps the result safe"
)]
fn compute_time_progress(resets_at: u64, window_seconds: u64, now: SystemTime) -> f64 {
    // `Date.now() / 1000` (ts:99). A pre-epoch clock cannot happen in
    // practice; fail soft to 0.0 rather than panic.
    let now_seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let window_start = resets_at as f64 - window_seconds as f64;
    let elapsed = now_seconds - window_start;
    (elapsed / window_seconds as f64).clamp(0.0, 1.0)
}

/// `getHeaderBasedEarlyWarning` (`claudeAiLimits.ts:255-294`): iterate the
/// claim map in order; the first claim whose
/// `anthropic-ratelimit-unified-{abbrev}-surpassed-threshold` header is
/// PRESENT (`!== null`, `:268`) wins and yields a FRESH replacement limits
/// object (`:281-289`) — only the fields the TS object sets are populated.
fn header_based_early_warning(
    headers: &[(String, String)],
    fallback_available: Option<bool>,
) -> Option<RateLimitInfo> {
    for (abbrev, rate_limit_type) in EARLY_WARNING_CLAIM_MAP {
        let surpassed_name = format!("anthropic-ratelimit-unified-{abbrev}-surpassed-threshold");
        if header_value(headers, &surpassed_name).is_none() {
            continue;
        }
        // `utilizationHeader ? Number(...) : undefined` (ts:276-279) — the
        // tolerant parsers reproduce the absent→undefined collapse.
        let resets_at = parse_epoch_secs(
            headers,
            &format!("anthropic-ratelimit-unified-{abbrev}-reset"),
        );
        return Some(RateLimitInfo {
            status: Some("allowed_warning".to_string()),
            // The TS fresh object folds the per-claim reset into the
            // top-level `resetsAt` (ts:283); our struct also splits out the
            // per-claim value, so the same timestamp lands in both.
            resets_at,
            claim_resets_at: resets_at,
            rate_limit_type: Some(rate_limit_type.to_string()),
            utilization: parse_fraction(
                headers,
                &format!("anthropic-ratelimit-unified-{abbrev}-utilization"),
            ),
            fallback_available,
            // `Number(surpassedThreshold)` (ts:288). Divergence (same
            // fail-soft stance as `parse_fraction` everywhere else): TS would
            // store `NaN` for a malformed value — but `0` for an EMPTY string
            // (`Number('')` is `0`, not `NaN`); we store `None` for both. The
            // warning itself still fires on header PRESENCE alone (ts:268).
            surpassed_threshold: parse_fraction(headers, &surpassed_name),
            // Fresh-object semantics: overage fields stay `None`
            // (`isUsingOverage: false` in TS has no struct counterpart).
            ..RateLimitInfo::default()
        });
    }
    None
}

/// `getTimeRelativeEarlyWarning` (`claudeAiLimits.ts:301-340`): client-side
/// fallback for one config. Requires BOTH per-claim headers (`:315`); warns
/// when ANY threshold has `utilization >= t.utilization && timeProgress <=
/// t.timePct` (`:324-326`). The fresh object carries NO `surpassedThreshold`
/// (`:332-339`).
fn time_relative_early_warning(
    headers: &[(String, String)],
    config: &EarlyWarningConfig,
    fallback_available: Option<bool>,
    now: SystemTime,
) -> Option<RateLimitInfo> {
    let abbrev = config.claim_abbrev;
    // TS gates on header PRESENCE (`=== null`) then `Number()`s the values; a
    // malformed value becomes `NaN`, every `NaN` comparison is false, and no
    // warning fires — requiring a successful parse here is behaviourally
    // identical and keeps the fail-soft stance of the tolerant parsers.
    let utilization = parse_fraction(
        headers,
        &format!("anthropic-ratelimit-unified-{abbrev}-utilization"),
    )?;
    let resets_at = parse_epoch_secs(
        headers,
        &format!("anthropic-ratelimit-unified-{abbrev}-reset"),
    )?;
    let time_progress = compute_time_progress(resets_at, config.window_seconds, now);
    let should_warn = config
        .thresholds
        .iter()
        .any(|t| utilization >= t.utilization && time_progress <= t.time_pct);
    if !should_warn {
        return None;
    }
    Some(RateLimitInfo {
        status: Some("allowed_warning".to_string()),
        // Same `resetsAt` fold as the header-based path (ts:334).
        resets_at: Some(resets_at),
        claim_resets_at: Some(resets_at),
        rate_limit_type: Some(config.rate_limit_type.to_string()),
        utilization: Some(utilization),
        fallback_available,
        ..RateLimitInfo::default()
    })
}

/// `getEarlyWarningFromHeaders` (`claudeAiLimits.ts:347-374`): header-based
/// detection first (preferred when the API sends the header), else the
/// time-relative configs in priority order.
fn early_warning_from_headers(
    headers: &[(String, String)],
    fallback_available: Option<bool>,
    now: SystemTime,
) -> Option<RateLimitInfo> {
    if let Some(warning) = header_based_early_warning(headers, fallback_available) {
        return Some(warning);
    }
    EARLY_WARNING_CONFIGS
        .iter()
        .find_map(|config| time_relative_early_warning(headers, config, fallback_available, now))
}

impl RateLimitInfo {
    /// Parse the unified rate-limit headers the 429 error-message path reads.
    /// 1:1 with the `error.headers?.get(...)` reads in claude-code
    /// `errors.ts:471-516` + `claudeAiLimits.ts` `computeNewLimitsFromHeaders`.
    /// Thin wrapper over [`Self::from_headers_at`] with the wall clock.
    #[must_use]
    pub fn from_headers(headers: &[(String, String)]) -> Self {
        Self::from_headers_at(headers, SystemTime::now())
    }

    /// Testable core of [`Self::from_headers`] with an injected `now` — the
    /// clock only feeds `computeTimeProgress` (`claudeAiLimits.ts:98-103`) on
    /// the time-relative early-warning fallback path.
    ///
    /// After the raw header parse, the `computeNewLimitsFromHeaders`
    /// final-status semantics apply (`claudeAiLimits.ts:411-424`): when the
    /// parsed status is `allowed`/`allowed_warning`, a firing early warning
    /// REPLACES the whole parse, and otherwise a bare `allowed_warning` is
    /// downgraded to `allowed`; `rejected` passes through untouched.
    #[must_use]
    pub fn from_headers_at(headers: &[(String, String)], now: SystemTime) -> Self {
        let rate_limit_type =
            header_value(headers, "anthropic-ratelimit-unified-representative-claim")
                .map(str::to_string);

        // Per-claim window reads keyed by the representative claim's abbrev
        // (`anthropic-ratelimit-unified-{abbrev}-utilization` / `-reset`,
        // claudeAiLimits.ts:164-179). No abbrev → no per-claim read.
        let abbrev = rate_limit_type.as_deref().and_then(claim_abbrev);
        let utilization = abbrev.and_then(|a| {
            parse_fraction(
                headers,
                &format!("anthropic-ratelimit-unified-{a}-utilization"),
            )
        });
        let claim_resets_at = abbrev.and_then(|a| {
            parse_epoch_secs(headers, &format!("anthropic-ratelimit-unified-{a}-reset"))
        });

        let mut parsed = Self {
            rate_limit_type,
            overage_status: header_value(headers, "anthropic-ratelimit-unified-overage-status")
                .map(str::to_string),
            overage_disabled_reason: overage_disabled_reason(headers).map(str::to_string),
            // `headers.get(…) || 'allowed'` (claudeAiLimits.ts:379-381) —
            // empty string is falsy in TS, so it is "no usable value" → None.
            status: header_value(headers, "anthropic-ratelimit-unified-status")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            resets_at: parse_epoch_secs(headers, "anthropic-ratelimit-unified-reset"),
            utilization,
            claim_resets_at,
            overage_resets_at: parse_epoch_secs(
                headers,
                "anthropic-ratelimit-unified-overage-reset",
            ),
            // `=== 'available'` (claudeAiLimits.ts:384-385): strict equality
            // on the verbatim header value — no trim, no case-fold.
            fallback_available: header_value(headers, "anthropic-ratelimit-unified-fallback")
                .map(|v| v == "available"),
            // Only ever set by the header-based early-warning replacement
            // below (claudeAiLimits.ts:288) — never by the raw parse.
            surpassed_threshold: None,
            // 2.1.206 overage fields — `=== 'true'` strict equality (no
            // header → `false`, matching the TS boolean's no-`undefined`
            // stance).
            overage_in_use: header_value(headers, "anthropic-ratelimit-unified-overage-in-use")
                == Some("true"),
            // `d ? d.split(',').map(trim) : undefined` — an empty header
            // value is falsy in TS, so a present-but-empty header collapses to
            // `None` exactly like an absent one (same `.filter(!empty)`
            // convention as `status` / the `from_429_error_headers` fields).
            upgrade_paths: header_value(headers, "anthropic-ratelimit-unified-upgrade-paths")
                .filter(|s| !s.is_empty())
                .map(|v| v.split(',').map(|s| s.trim().to_string()).collect()),
            overage_period_monthly_utilization: parse_fraction(
                headers,
                "anthropic-ratelimit-unified-overage-period-monthly-utilization",
            ),
            overage_period_channel_utilization: parse_fraction(
                headers,
                "anthropic-ratelimit-unified-overage-period-channel-utilization",
            ),
            // The success-path header parse never sees a 429 error body —
            // `Nqi(e)` only runs on the error-catch site
            // ([`Self::from_429_error`]).
            credits_required: false,
        };

        // Final-status semantics (claudeAiLimits.ts:411-424) — the early
        // warning is only consulted when status is allowed/allowed_warning.
        match parsed.status.as_deref() {
            Some("allowed" | "allowed_warning") => {
                if let Some(warning) =
                    early_warning_from_headers(headers, parsed.fallback_available, now)
                {
                    // TS RETURNS the fresh early-warning object (ts:419-421),
                    // discarding the regular parse — including every overage
                    // field. Every field on this struct maps to a
                    // `ClaudeAILimits` member (claudeAiLimits.ts:122-136), so
                    // there are no transport-only fields to carry over.
                    return warning;
                }
                // No early-warning threshold surpassed → a bare
                // allowed_warning is DOWNGRADED (ts:423 `finalStatus = 'allowed'`).
                parsed.status = Some("allowed".to_string());
                parsed
            }
            // `Some(_)`: 'rejected' (and any other non-empty value) passes
            // through — ts:413 keeps `finalStatus = status` outside the
            // allowed branch.
            //
            // `None`: deliberate divergence-guard. TS defaults a missing
            // status to 'allowed' (ts:379-381), which would route it through
            // the branch above — but fabricating a status with zero unified
            // headers present would break `has_unified_headers()` consumers,
            // so an absent header stays `None` (readers apply
            // `.unwrap_or("allowed")`). Note the guard's reach: it suppresses
            // the early-warning check for ANY response missing the status
            // header — even one carrying other unified headers, where TS's
            // 'allowed' default could still fire a warning — not just the
            // zero-header case. Improbable in practice: the server sends the
            // status header alongside the per-claim headers.
            Some(_) | None => parsed,
        }
    }

    /// `true` when at least one unified-limit header is present — matches the
    /// claude-code gate `if (rateLimitType || overageStatus)` (`errors.ts:480`)
    /// that decides whether the new message generator runs at all.
    #[must_use]
    pub fn has_unified_headers(&self) -> bool {
        self.rate_limit_type.is_some() || self.overage_status.is_some()
    }

    /// Build the rejected-limits view from a 429 **error** response's headers
    /// — 1:1 with claude-code `errors.ts:471-516`, which constructs a FRESH
    /// `ClaudeAILimits` object directly from `error.headers` (it does NOT run
    /// `computeNewLimitsFromHeaders`, so no early-warning replacement and no
    /// final-status downgrade apply here).
    ///
    /// Returns `None` when neither the representative-claim nor the
    /// overage-status header carries a non-empty value — the
    /// `if (rateLimitType || overageStatus)` gate (`errors.ts:480`; an empty
    /// header value is falsy in TS). On `Some`, `status` is FORCED to
    /// `"rejected"` (`errors.ts:482-486` builds `{ status: 'rejected', … }`)
    /// and only the five fields the TS object sets are populated:
    /// `resetsAt`/`rateLimitType`/`overageStatus`/`overageResetsAt`/
    /// `overageDisabledReason` (`errors.ts:488-517`, each behind its own
    /// truthiness check).
    #[must_use]
    pub fn from_429_error_headers(headers: &[(String, String)]) -> Option<Self> {
        Self::from_429_error(headers, None)
    }

    /// [`Self::from_429_error_headers`] plus claude-code `Nqi(e)`: when the
    /// 429's parsed JSON error `body` carries
    /// `error.error.details.error_code === "credits_required"`, sets
    /// [`Self::credits_required`] and — when no header already supplied one —
    /// uses `details.disabled_reason` (if a string) as
    /// [`Self::overage_disabled_reason`]. The header value takes precedence
    /// over the body's when both are present (the header-driven
    /// `overage_disabled_reason()` parse runs first below).
    ///
    /// `body` is independent of the header gate (`errors.ts:480`): a
    /// `credits_required` body alone is enough to pass the gate and produce
    /// `Some`, even with no unified headers on the response, since claude-code
    /// derives it from a separate error-catch site (`Nqi`) than the header
    /// object built at `errors.ts:471-516`.
    #[must_use]
    pub fn from_429_error(
        headers: &[(String, String)],
        body: Option<&serde_json::Value>,
    ) -> Option<Self> {
        // `headers?.get?.(…)` + TS truthiness: empty string is falsy, so it
        // neither passes the gate nor is assigned onto the limits object.
        let rate_limit_type =
            header_value(headers, "anthropic-ratelimit-unified-representative-claim")
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        let overage_status = header_value(headers, "anthropic-ratelimit-unified-overage-status")
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (credits_required, body_disabled_reason) = body
            .map(Self::credits_required_from_body)
            .unwrap_or((false, None));
        if rate_limit_type.is_none() && overage_status.is_none() && !credits_required {
            return None;
        }
        Some(Self {
            // errors.ts:483 — status is 'rejected' regardless of any
            // `anthropic-ratelimit-unified-status` header on the error.
            status: Some("rejected".to_string()),
            rate_limit_type,
            overage_status,
            // `if (resetHeader) limits.resetsAt = Number(resetHeader)`
            // (errors.ts:489-494) — the tolerant parse reproduces the
            // absent/empty→skip collapse; malformed values fail soft to
            // `None` (the documented divergence from TS storing `NaN`).
            resets_at: parse_epoch_secs(headers, "anthropic-ratelimit-unified-reset"),
            overage_resets_at: parse_epoch_secs(
                headers,
                "anthropic-ratelimit-unified-overage-reset",
            ),
            // `if (overageDisabledReason)` (errors.ts:511-516) — empty is falsy.
            // Header takes precedence over the `Nqi` body-derived reason.
            overage_disabled_reason: overage_disabled_reason(headers)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or(body_disabled_reason),
            credits_required,
            ..Self::default()
        })
    }

    /// Dig a 429 error body for claude-code `Nqi(e)`'s
    /// `error.error.details` — returns `(credits_required,
    /// disabled_reason)`. `disabled_reason` is only meaningful when
    /// `credits_required` is `true` (mirrors `Nqi` only reading
    /// `details.disabled_reason` inside the `error_code==="credits_required"`
    /// branch).
    fn credits_required_from_body(body: &serde_json::Value) -> (bool, Option<String>) {
        let details = body
            .get("error")
            .and_then(|e| e.get("error"))
            .and_then(|e| e.get("details"));
        let Some(details) = details else {
            return (false, None);
        };
        let is_credits_required = details
            .get("error_code")
            .and_then(serde_json::Value::as_str)
            == Some("credits_required");
        if !is_credits_required {
            return (false, None);
        }
        let disabled_reason = details
            .get("disabled_reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        (true, disabled_reason)
    }
}

/// Pre-formatted reset-time strings threaded into the 429 message template.
///
/// claude-code derives these from `formatResetTime(limits.resetsAt, true)` and
/// `formatResetTime(limits.overageResetsAt, true)` (`rateLimitMessages.ts:144-147`).
/// That formatter is locale/timezone dependent, so the api-client layer accepts
/// the already-formatted strings and only owns the byte-locked surrounding
/// template. Pass `None` when the corresponding reset timestamp was absent.
#[derive(Debug, Clone, Default)]
pub struct ResetTimes<'a> {
    /// Formatted `limits.resetsAt` (the primary window reset), if present.
    pub reset_time: Option<&'a str>,
    /// Formatted `limits.overageResetsAt` (the overage window reset), if present.
    pub overage_reset_time: Option<&'a str>,
    /// `true` when the *unformatted* `resetsAt` timestamp was present — used by
    /// the dual-reset overage branch to pick the earlier of the two windows.
    /// claude-code compares the raw `resetsAt < overageResetsAt` numbers
    /// (`rateLimitMessages.ts:155-166`); the caller passes the comparison result
    /// via [`Self::reset_is_earlier`] so this layer stays format-agnostic.
    pub reset_is_earlier: Option<bool>,
}

impl<'a> ResetTimes<'a> {
    /// Pick the reset message for the dual-overage-rejected branch
    /// (claude-code `rateLimitMessages.ts:154-166`): when both reset times are
    /// present, use the earlier window's formatted string (per
    /// `reset_is_earlier`); otherwise fall back to whichever single one exists.
    fn overage_reset_message(&self) -> String {
        match (self.reset_time, self.overage_reset_time) {
            (Some(rt), Some(ort)) => {
                // Both present: claude-code uses the earlier of resetsAt /
                // overageResetsAt. `reset_is_earlier` carries that numeric
                // comparison; default to the primary reset when unknown.
                if self.reset_is_earlier.unwrap_or(true) {
                    format!(" · resets {rt}")
                } else {
                    format!(" · resets {ort}")
                }
            }
            (Some(rt), None) => format!(" · resets {rt}"),
            (None, Some(ort)) => format!(" · resets {ort}"),
            (None, None) => String::new(),
        }
    }
}

/// Whether the running user is a Pro or Enterprise subscriber — gates the
/// `seven_day_sonnet` wording (claude-code `rateLimitMessages.ts:176-181`,
/// `getSubscriptionType() === 'pro' || 'enterprise'`). Pre-computed by the
/// caller (subscription state lives outside api-client) and handed in, the same
/// seam as `is_subscriber` / `is_enterprise` on the retry path.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubscriptionContext {
    /// `true` when `getSubscriptionType()` is `pro` or `enterprise`.
    pub is_pro_or_enterprise: bool,
}

/// Build the byte-faithful user-facing 429 rate-limit error message from the
/// rejected-limit state — the **error** (not warning) branch of claude-code
/// `getRateLimitErrorMessage` → `getRateLimitMessage` → `getLimitReachedText`
/// (`rateLimitMessages.ts:110-197`). Returns `None` when no error message
/// applies (the TS warning/overage paths that return `null`), letting the
/// caller fall through to its generic 429 handling.
///
/// Byte-locked templates (external build; the `USER_TYPE === 'ant'` feedback
/// channel variant at `:339-340` is omitted — same external-only stance as the
/// rest of api-client). `reset` carries the locale-formatted reset strings.
#[must_use]
pub fn rate_limit_error_message(
    info: &RateLimitInfo,
    reset: &ResetTimes<'_>,
    sub: SubscriptionContext,
) -> Option<String> {
    // claude-code builds `limits` with status='rejected' and isUsingOverage
    // defaulted false (errors.ts:482-486). getRateLimitMessage therefore skips
    // the `isUsingOverage` branch and the `allowed_warning` warning branch, and
    // lands directly on the rejected → getLimitReachedText path
    // (rateLimitMessages.ts:63-64). getRateLimitErrorMessage only returns the
    // message when severity === 'error', which this path always is.
    Some(limit_reached_text(info, reset, sub))
}

/// Port of claude-code `getLimitReachedText` (`rateLimitMessages.ts:143-197`).
fn limit_reached_text(
    info: &RateLimitInfo,
    reset: &ResetTimes<'_>,
    sub: SubscriptionContext,
) -> String {
    // `const resetMessage = resetTime ? ` · resets ${resetTime}` : ''` (:149).
    let reset_message = reset
        .reset_time
        .map(|rt| format!(" · resets {rt}"))
        .unwrap_or_default();

    // if BOTH subscription and overage are exhausted (:152).
    if info.overage_status.as_deref() == Some("rejected") {
        let overage_reset_message = reset.overage_reset_message();
        // `out_of_credits` → "You're out of extra usage…" (:168-170).
        if info.overage_disabled_reason.as_deref() == Some("out_of_credits") {
            return format!("You're out of extra usage{overage_reset_message}");
        }
        // else formatLimitReachedText('limit', overageResetMessage) (:172).
        return format_limit_reached_text("limit", &overage_reset_message);
    }

    match info.rate_limit_type.as_deref() {
        Some("seven_day_sonnet") => {
            // pro/enterprise: Sonnet limit is the weekly limit (:176-181).
            let limit = if sub.is_pro_or_enterprise {
                "weekly limit"
            } else {
                "Sonnet limit"
            };
            format_limit_reached_text(limit, &reset_message)
        }
        Some("seven_day_opus") => format_limit_reached_text("Opus limit", &reset_message),
        Some("seven_day") => format_limit_reached_text("weekly limit", &reset_message),
        Some("five_hour") => format_limit_reached_text("session limit", &reset_message),
        _ => format_limit_reached_text("usage limit", &reset_message),
    }
}

/// Port of claude-code `formatLimitReachedText` (`rateLimitMessages.ts:333-344`),
/// external build only — the `USER_TYPE === 'ant'` feedback-channel variant is
/// intentionally omitted (api-client is the external CLI surface).
fn format_limit_reached_text(limit: &str, reset_message: &str) -> String {
    format!("You've hit your {limit}{reset_message}")
}

/// Parse the strict `YYYY-MM-DDTHH:MM:SSZ` form into a Unix-epoch second.
/// Hand-rolled — no chrono runtime dep needed.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::similar_names,
    clippy::unreadable_literal,
    reason = "Hinnant days_from_civil algorithm: ranges are mathematically bounded by the YYYY-MM-DDTHH:MM:SSZ validation above; literals 146097/719468 are canonical algorithm constants"
)]
fn parse_iso8601_utc(s: &str) -> Option<u64> {
    // 1234567890123456789012345
    // YYYY-MM-DDTHH:MM:SSZ
    if s.len() != 20 || !s.ends_with('Z') {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u32 = s[11..13].parse().ok()?;
    let minute: u32 = s[14..16].parse().ok()?;
    let second: u32 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour >= 24
        || minute >= 60
        || second >= 60
    {
        return None;
    }
    // Days since Unix epoch (1970-01-01), using the proleptic Gregorian
    // calendar. We only need to handle 1970-2099 for our use case; the
    // formula is the standard "civil from days" inverse.
    let y = year;
    let m = i64::from(month);
    let d = i64::from(day);
    // Howard Hinnant's "days_from_civil" algorithm:
    let y_adj = y - i64::from(m <= 2);
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = (y_adj - era * 400) as u64; // [0, 399]
    let doy: u64 = (153 * {
        let m = m as u64;
        if m > 2 {
            m - 3
        } else {
            m + 9
        }
    } + 2)
        / 5
        + d as u64
        - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe as i64 - 719468; // 719468 = days from 0000-03-01 to 1970-01-01
    if days < 0 {
        return None;
    }
    let secs = (days as u64) * 86_400
        + u64::from(hour) * 3_600
        + u64::from(minute) * 60
        + u64::from(second);
    Some(secs)
}

/// Seconds to wait, from an llm-runtime rate-limit error (server value wins).
///
/// Returns `Some(secs)` for a `RateLimited` error with a `retry_after` duration
/// (minimum 1 second), `None` for `RateLimited` with no duration, and `None`
/// for all other error types.
///
/// Note: the retry driver sleeps on the `LlmError`'s Duration verbatim; this helper is only for user-facing display.
#[must_use]
pub fn retry_secs_from_error(error: &crate::LlmError) -> Option<u64> {
    match error {
        crate::LlmError::RateLimited {
            retry_after: Some(d),
            ..
        } => Some(d.as_secs().max(1)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "rate_limit_test.rs"]
mod rate_limit_test;
