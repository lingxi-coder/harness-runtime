//! UTC ISO-8601 timestamps with millisecond precision — the Rust equivalent of
//! JS `new Date(ms).toISOString()`.
//!
//! This lives in `protocol` because four separate call sites had grown their
//! own copy of the identical civil-from-days algorithm (`tools/ui/brief.rs`,
//! `tools/ui/push_notification.rs`, `tools/task/task.rs`, and the `mcp xaa
//! login` expiry line). They agreed, which is exactly why the duplication was
//! invisible: nothing failed, so nothing pointed at it.
//!
//! Std-only and deterministic for a fixed [`SystemTime`], so callers can format
//! an injected clock's output in a test without a time crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Civil date (year, month, day) from a count of days since the Unix epoch.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the full proleptic
/// Gregorian range rather than only for dates near the epoch.
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format `t` as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// A time before the Unix epoch clamps to the epoch rather than panicking or
/// producing a negative-year string: these timestamps go into user-facing
/// output and log lines, where a wrong-but-well-formed date is far less
/// damaging than a crash in an error path.
#[must_use]
pub fn iso8601_utc(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats_as_zero() {
        assert_eq!(iso8601_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_value_matches_js_to_iso_string() {
        // `new Date(1749300896789).toISOString()` === "2025-06-07T12:54:56.789Z"
        let t = UNIX_EPOCH + Duration::from_millis(1_749_300_896_789);
        assert_eq!(iso8601_utc(t), "2025-06-07T12:54:56.789Z");
    }

    #[test]
    fn leap_day_is_exact() {
        // 2024-02-29T00:00:00Z
        let t = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(iso8601_utc(t), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn pre_epoch_clamps_instead_of_panicking() {
        let t = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(iso8601_utc(t), "1970-01-01T00:00:00.000Z");
    }
}
