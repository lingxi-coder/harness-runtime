//! Spec §7 line 721: proactive refresh lead is `min(remaining / 2, 5 * 60)` seconds.
//!
//! Edge cases worth locking:
//! - remaining = 1 hour -> lead = 5 min (the cap)
//! - remaining = 8 min  -> lead = 4 min (remaining/2)
//! - remaining = 30 sec -> lead = 15 sec (debug-token short TTL)
//! - remaining = 0      -> lead = 0 (refresh immediately)

use llm_runtime::auth::anthropic::refresh::proactive_lead;
use std::time::Duration;

#[test]
fn lead_caps_at_5_minutes_for_long_remaining() {
    assert_eq!(
        proactive_lead(Duration::from_secs(3600)),
        Duration::from_secs(300)
    );
    assert_eq!(
        proactive_lead(Duration::from_secs(86400)),
        Duration::from_secs(300)
    );
}

#[test]
fn lead_is_half_remaining_when_short() {
    assert_eq!(
        proactive_lead(Duration::from_secs(480)),
        Duration::from_secs(240)
    );
    assert_eq!(
        proactive_lead(Duration::from_secs(120)),
        Duration::from_secs(60)
    );
}

#[test]
fn lead_is_half_for_sub_minute_tokens() {
    assert_eq!(
        proactive_lead(Duration::from_secs(30)),
        Duration::from_secs(15)
    );
    assert_eq!(
        proactive_lead(Duration::from_secs(2)),
        Duration::from_secs(1)
    );
}

#[test]
fn lead_is_zero_for_already_expired() {
    assert_eq!(proactive_lead(Duration::ZERO), Duration::ZERO);
}

#[test]
fn lead_at_exactly_600_seconds_is_300() {
    // remaining = 600s, remaining/2 = 300s, cap = 300s -> both equal -> 300.
    assert_eq!(
        proactive_lead(Duration::from_secs(600)),
        Duration::from_secs(300)
    );
}

#[test]
fn lead_just_under_600s_uses_half() {
    // remaining = 599s, remaining/2 = 299s (less than cap 300s) -> 299s.
    assert_eq!(
        proactive_lead(Duration::from_secs(599)),
        Duration::from_secs(299)
    );
}
