//! MCP reconnect backoff schedule parity with claude-code
//! `useManageMCPConnections.ts:372-461` (`MAX_RECONNECT_ATTEMPTS = 5`,
//! `INITIAL_BACKOFF_MS = 1000`, `MAX_BACKOFF_MS = 30000`).
use super::{backoff_for, post_attempt_backoff, INITIAL_BACKOFF, MAX_BACKOFF};
use std::time::Duration;

/// The ordered list of inter-attempt sleeps actually taken during a
/// reconnect run of `max` attempts — derived from the same
/// [`post_attempt_backoff`] decision the production loop uses.
fn sleep_schedule(max: u32) -> Vec<Duration> {
    (1..=max)
        .filter_map(|attempt| post_attempt_backoff(attempt, max))
        .collect()
}

#[test]
fn constants_match_claude_code() {
    assert_eq!(INITIAL_BACKOFF, Duration::from_millis(1000));
    assert_eq!(MAX_BACKOFF, Duration::from_secs(30));
}

#[test]
fn no_leading_sleep_and_no_trailing_sleep_on_final_attempt() {
    let max = 5;
    // The final attempt never sleeps afterwards (no trailing sleep).
    assert_eq!(post_attempt_backoff(max, max), None);
    // Every non-final attempt sleeps. The first inter-attempt sleep happens
    // AFTER attempt 1 — the loop calls `connect` before any sleep, so
    // attempt 1 has no leading sleep.
    for attempt in 1..max {
        assert!(
            post_attempt_backoff(attempt, max).is_some(),
            "attempt {attempt} of {max} should be followed by a backoff",
        );
    }
    // Exactly `max - 1` sleeps occur across the whole run.
    assert_eq!(sleep_schedule(max).len(), (max - 1) as usize);
}

#[test]
fn schedule_is_1_2_4_8_seconds_and_16s_is_never_slept() {
    // claude-code sleeps 1s, 2s, 4s, 8s between the 5 attempts. The 16s
    // value (and the 30s cap) is NEVER used — it would only ever be a
    // trailing sleep after the final attempt, which does not exist.
    assert_eq!(
        sleep_schedule(5),
        vec![
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
        ],
    );
    assert!(
        !sleep_schedule(5).contains(&Duration::from_secs(16)),
        "the 16s backoff (attempt 5) must never be slept",
    );
}

#[test]
fn attempt_fire_times_are_0_1_3_7_15_seconds() {
    // Cumulative offsets at which each of the 5 attempts fires. Attempt 1
    // at t = 0 proves there is no leading sleep.
    let mut fire_times = vec![Duration::ZERO];
    let mut acc = Duration::ZERO;
    for s in sleep_schedule(5) {
        acc += s;
        fire_times.push(acc);
    }
    assert_eq!(
        fire_times,
        vec![
            Duration::from_secs(0),
            Duration::from_secs(1),
            Duration::from_secs(3),
            Duration::from_secs(7),
            Duration::from_secs(15),
        ],
    );
}

#[test]
fn backoff_for_is_exponential_and_capped_at_max() {
    assert_eq!(backoff_for(1), Duration::from_secs(1));
    assert_eq!(backoff_for(2), Duration::from_secs(2));
    assert_eq!(backoff_for(3), Duration::from_secs(4));
    assert_eq!(backoff_for(4), Duration::from_secs(8));
    assert_eq!(backoff_for(5), Duration::from_secs(16));
    // 1000 * 2^5 = 32s exceeds the 30s ceiling → clamped.
    assert_eq!(backoff_for(6), MAX_BACKOFF);
    // Large attempts saturate at the cap, never overflow.
    assert_eq!(backoff_for(100), MAX_BACKOFF);
}
