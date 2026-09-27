//! `tengu_kairos_*` and `tengu_loop_*` event name constants — the `/loop`
//! (Kairos) autonomous-loop subsystem.
//!
//! Like [`workflow`](crate::tengu::workflow) these are NOT added to the
//! count-locked `ALL_EVENT_NAMES` / `tengu_events.json` fixture (that snapshot is
//! from an OLDER claude event set; adding these would break the 363-entry
//! byte-parity lock). They live here for string-lock testing only.
//!
//! Two families:
//!   - `tengu_kairos_*` — feature-area events. `pJr()`
//!     (`logAutonomousLoopActivation`, cc_all.txt:504950) emits
//!     `W("tengu_kairos_loop_persistent_activated",{variant})`.
//!   - `tengu_loop_*` — loop-lifecycle/scheduling events (`Vst`/`aKi`/`cKi`/`lKi`,
//!     cc_all.txt). `Le()` (the reason mapper) is an identity passthrough, so the
//!     `reason` field is the raw literal.

// ── tengu_kairos_* (feature-area) ────────────────────────────────────────────

/// `tengu_kairos_loop_persistent_activated` — the autonomous-loop default was
/// activated by the `/loop` command (or a fire-time resolver). `variant` carries
/// `isLoopPersistentPreambleEnabled()`.
pub const LOOP_PERSISTENT_ACTIVATED: &str = "tengu_kairos_loop_persistent_activated";

// ── tengu_loop_* (lifecycle / scheduling) ────────────────────────────────────

/// `tengu_loop_ended` (binary `Vst`) — a `/loop` ended. `reason` is the raw
/// literal (`Le()` is identity): `gate_off` | `model_stopped` | `aged_out` |
/// `user_abort`, plus mode-specific extras (`via_keepalive`, `loops_cancelled`).
pub const LOOP_ENDED: &str = "tengu_loop_ended";

/// `tengu_loop_dynamic_wakeup_scheduled` (binary `cKi`) — a dynamic-pacing
/// `ScheduleWakeup` was scheduled. Fields: `chosen_delay_seconds`,
/// `clamped_delay_seconds`, `was_clamped`, `reason_length`, `superseded_count`.
pub const LOOP_DYNAMIC_WAKEUP_SCHEDULED: &str = "tengu_loop_dynamic_wakeup_scheduled";

/// `tengu_loop_keepalive_fired` (binary `lKi`) — the keepalive fallback heartbeat
/// re-armed the loop. Fields: `clamped_delay_seconds`, `prompt_is_sentinel`.
/// NOTE: emitting this requires the keepalive scheduling machinery (still pending).
pub const LOOP_KEEPALIVE_FIRED: &str = "tengu_loop_keepalive_fired";

/// `tengu_loop_dynamic_wakeup_aged_out` (binary age-out path) — a dynamic loop
/// exceeded `recurringMaxAgeMs`. Fields: `loop_age_ms`, `max_age_ms`.
/// NOTE: emitting this requires the age-out machinery (still pending).
pub const LOOP_DYNAMIC_WAKEUP_AGED_OUT: &str = "tengu_loop_dynamic_wakeup_aged_out";

/// `tengu_loop_dynamic_wakeup_ends_turn` (binary turn-loop branch) — a lone
/// `ScheduleWakeup` that armed a `/loop` wakeup ended the turn. Fields:
/// `queryChainId`, `queryDepth`. Emitted by `turn_loop`'s lone-wakeup arm.
pub const LOOP_DYNAMIC_WAKEUP_ENDS_TURN: &str = "tengu_loop_dynamic_wakeup_ends_turn";

/// `tengu_push_notification_send` (binary `PushNotification` tool `call`) — a
/// notification was sent (or suppressed). Fields: `message_length`, `push_sent`,
/// `local_sent`, `is_remote`, `disabled_reason`.
pub const PUSH_NOTIFICATION_SEND: &str = "tengu_push_notification_send";

/// Every reachable kairos/loop/push telemetry event name (string-lock only, NOT
/// in `ALL_EVENT_NAMES`).
pub const NAMES: &[&str] = &[
    LOOP_PERSISTENT_ACTIVATED,
    LOOP_ENDED,
    LOOP_DYNAMIC_WAKEUP_SCHEDULED,
    LOOP_KEEPALIVE_FIRED,
    LOOP_DYNAMIC_WAKEUP_AGED_OUT,
    LOOP_DYNAMIC_WAKEUP_ENDS_TURN,
    PUSH_NOTIFICATION_SEND,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_kairos_loop_or_push_prefixed() {
        for n in NAMES {
            assert!(
                n.starts_with("tengu_kairos_")
                    || n.starts_with("tengu_loop_")
                    || n.starts_with("tengu_push_"),
                "{n} must be tengu_kairos_* / tengu_loop_* / tengu_push_*"
            );
        }
    }

    #[test]
    fn event_names_are_byte_exact() {
        // PARITY: binary event-name literals (cc_all.txt:504950 / Vst / cKi / lKi).
        assert_eq!(
            LOOP_PERSISTENT_ACTIVATED,
            "tengu_kairos_loop_persistent_activated"
        );
        assert_eq!(LOOP_ENDED, "tengu_loop_ended");
        assert_eq!(
            LOOP_DYNAMIC_WAKEUP_SCHEDULED,
            "tengu_loop_dynamic_wakeup_scheduled"
        );
        assert_eq!(LOOP_KEEPALIVE_FIRED, "tengu_loop_keepalive_fired");
        assert_eq!(
            LOOP_DYNAMIC_WAKEUP_AGED_OUT,
            "tengu_loop_dynamic_wakeup_aged_out"
        );
        assert_eq!(PUSH_NOTIFICATION_SEND, "tengu_push_notification_send");
        assert_eq!(
            LOOP_DYNAMIC_WAKEUP_ENDS_TURN,
            "tengu_loop_dynamic_wakeup_ends_turn"
        );
    }
}
