//! Auto-mode classifier **denial circuit breaker** — 1:1 with claude-code.
//!
//! When the auto-mode (transcript / YOLO) classifier auto-DENIES a tool call,
//! claude-code increments a pair of counters and, once either crosses a
//! threshold, *trips the breaker*: it stops auto-denying and falls back to
//! interactive prompting (CLI), or aborts the run (headless). This module ports
//! that breaker byte-for-byte.
//!
//! # Source of truth (claude-code v2.1.183)
//!
//! Breaker primitives (binary offset ~202971002):
//! ```js
//! function T6n(){return{consecutiveDenials:0,totalDenials:0}}
//! function oel(e){return{...e,consecutiveDenials:e.consecutiveDenials+1,totalDenials:e.totalDenials+1}}
//! function Y4t(e){if(e.consecutiveDenials===0)return e;return{...e,consecutiveDenials:0}}
//! function sel(e){return e.consecutiveDenials>=y6n.maxConsecutive||e.totalDenials>=y6n.maxTotal}
//! var y6n;var Mho=b(()=>{y6n={maxConsecutive:3,maxTotal:20}})
//! ```
//!
//! Breaker consumer `dSm` (binary offset ~205930031):
//! ```js
//! function dSm(e,t,n,r,o,s){
//!   if(!sel(e))return null;
//!   let i=e.totalDenials>=y6n.maxTotal,
//!       a=Fr(s).shouldAvoidPermissionPrompts,
//!       l=e.totalDenials, c=e.consecutiveDenials,
//!       u=i?`${l} actions were blocked this session. Please review the transcript before continuing.`
//!          :`${c} consecutive actions were blocked. Please review the transcript before continuing.`;
//!   if(j("tengu_auto_mode_denial_limit_exceeded",{limit:Qe(i?"total":"consecutive"),mode:Qe(a?"headless":"cli"),messageID:n.message.id,consecutiveDenials:c,totalDenials:l,toolName:Qi(r.name)}),a)
//!     throw new vu("Agent aborted: too many classifier denials in headless mode");
//!   if(C(`Classifier denial limit exceeded, falling back to prompting: ${u}`,{level:"warn"}),i)
//!     uAt(s,{...e,totalDenials:0,consecutiveDenials:0});
//!   let d=o.decisionReason?.type==="classifier"?o.decisionReason.classifier:"auto-mode";
//!   return{...o,decisionReason:{type:"classifier",classifier:d,reason:`${u}\n\nLatest blocked action: ${t}`}}
//! }
//! ```
//!
//! `oel` (increment) is called when the auto-mode classifier blocks an action
//! (binary: ``Auto mode classifier blocked action: ${T.reason}``), then `dSm`
//! checks/trips. `Y4t` (reset) is called when the mode is `"auto"` and there are
//! denials, or when a permission resolves to `allow` (`P.behavior==="allow"`).
//!
//! # Live consumer status in `LingXi` (PERM-AUTO-07)
//!
//! The auto-mode LLM classifier is now **LIVE**:
//! [`crate::classifier::is_classifier_permissions_enabled`] returns `true`, and
//! [`crate::policy_gate`]'s `auto_mode_classifier_result` consumes it — every
//! classifier `Deny` calls [`record_auto_deny`] and consults the breaker, so the
//! breaker genuinely trips in an auto-mode session. (The earlier "not present in
//! the external build / residual seam" note was stale.)
//!
//! On a trip the consumer now emits [`DENIAL_LIMIT_EVENT`]
//! (`telemetry::emit_auto_mode_denial_limit_exceeded`), logs
//! [`DenialBreakerTrip::fallback_warn_line`], and either aborts headless runs or
//! falls back to prompting. The headless abort and interactive rewritten reason
//! both travel through typed gate results, so no shared side-channel is needed.
//!
//! [`record_auto_deny`]: DenialTrackingState::record_auto_deny

/// Tunables — 1:1 with claude-code `y6n = {maxConsecutive:3,maxTotal:20}`.
pub mod limits {
    /// Consecutive auto-denials that trip the breaker (`y6n.maxConsecutive`).
    pub const MAX_CONSECUTIVE: u32 = 3;
    /// Total auto-denials this session that trip the breaker (`y6n.maxTotal`).
    pub const MAX_TOTAL: u32 = 20;
}

/// Telemetry event name fired on trip — 1:1 with claude-code
/// `tengu_auto_mode_denial_limit_exceeded`. Re-exported from [`telemetry`] so
/// there is one authoritative literal; see
/// [`telemetry::tengu::agent::AUTO_MODE_DENIAL_LIMIT_EXCEEDED`].
pub use telemetry::tengu::agent::AUTO_MODE_DENIAL_LIMIT_EXCEEDED as DENIAL_LIMIT_EVENT;

/// Flat auto-mode-classifier denial state — 1:1 with claude-code's
/// `{consecutiveDenials, totalDenials}` (`T6n()` initial value). NO per-tool map
/// and NO TTL: claude-code tracks only these two flat counters for the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DenialTrackingState {
    /// Auto-denials in an unbroken run (reset to 0 on any non-deny / allow).
    pub consecutive_denials: u32,
    /// Auto-denials accumulated this session (only reset when the *total* limit
    /// trips and the breaker falls back — see [`Self::trip`]).
    pub total_denials: u32,
}

/// Which limit tripped the breaker — maps to the `limit` telemetry tag
/// (`"total"` vs `"consecutive"`) and selects the trip message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialLimit {
    /// `consecutiveDenials >= maxConsecutive` (and total has not).
    Consecutive,
    /// `totalDenials >= maxTotal` — the dominant case when both are tripped.
    Total,
}

impl DenialLimit {
    /// The `limit:` telemetry tag value — 1:1 with claude-code `i?"total":"consecutive"`.
    #[must_use]
    pub const fn telemetry_tag(self) -> &'static str {
        match self {
            DenialLimit::Total => "total",
            DenialLimit::Consecutive => "consecutive",
        }
    }
}

/// What the consumer must do when the breaker trips — the structured form of
/// claude-code's `dSm` trip body (telemetry, then abort-or-fallback, then the
/// rewritten classifier decision-reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialBreakerTrip {
    /// Which limit fired (`DenialLimit::Total` dominates when both fire).
    pub limit: DenialLimit,
    /// `true` => headless / `shouldAvoidPermissionPrompts`: the run must ABORT.
    /// `false` => CLI: fall back to interactive prompting.
    pub headless: bool,
    /// `consecutiveDenials` at trip time (telemetry `consecutiveDenials`).
    pub consecutive_denials: u32,
    /// `totalDenials` at trip time (telemetry `totalDenials`).
    pub total_denials: u32,
    /// The user-facing summary line — 1:1 with claude-code `u`:
    /// total → `"{n} actions were blocked this session. …"`,
    /// consecutive → `"{n} consecutive actions were blocked. …"`.
    pub message: String,
}

impl DenialBreakerTrip {
    /// The byte-exact headless abort message — 1:1 with claude-code
    /// `new vu("Agent aborted: too many classifier denials in headless mode")`.
    /// Only meaningful when [`Self::headless`] is `true`.
    pub const HEADLESS_ABORT_MESSAGE: &'static str =
        "Agent aborted: too many classifier denials in headless mode";

    /// `mode:` telemetry tag — 1:1 with claude-code `a?"headless":"cli"`.
    #[must_use]
    pub const fn mode_tag(&self) -> &'static str {
        if self.headless {
            "headless"
        } else {
            "cli"
        }
    }

    /// The CLI fallback warn line — 1:1 with claude-code
    /// ``Classifier denial limit exceeded, falling back to prompting: ${u}``.
    #[must_use]
    pub fn fallback_warn_line(&self) -> String {
        format!(
            "Classifier denial limit exceeded, falling back to prompting: {}",
            self.message
        )
    }

    /// The rewritten classifier `reason` for the decision the consumer returns —
    /// 1:1 with claude-code ``${u}\n\nLatest blocked action: ${t}`` where `t`
    /// is the latest blocked action's description.
    #[must_use]
    pub fn decision_reason(&self, latest_blocked_action: &str) -> String {
        format!(
            "{}\n\nLatest blocked action: {}",
            self.message, latest_blocked_action
        )
    }
}

impl DenialTrackingState {
    /// Fresh state — 1:1 with claude-code `T6n()` (`{consecutiveDenials:0,totalDenials:0}`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            consecutive_denials: 0,
            total_denials: 0,
        }
    }

    /// Record one auto-mode classifier DENIAL — 1:1 with claude-code `oel`
    /// (`consecutiveDenials+1`, `totalDenials+1`). Saturating to be panic-safe
    /// (claude-code's JS numbers never overflow at these magnitudes; saturation
    /// preserves the "tripped" state past the threshold either way).
    pub fn record_auto_deny(&mut self) {
        self.consecutive_denials = self.consecutive_denials.saturating_add(1);
        self.total_denials = self.total_denials.saturating_add(1);
    }

    /// Record a NON-denial (an allow / ask-allowed) — 1:1 with claude-code `Y4t`:
    /// reset `consecutiveDenials` to 0 (leaving `totalDenials` untouched). A
    /// no-op when already 0, matching `if(e.consecutiveDenials===0)return e`.
    pub fn record_non_deny(&mut self) {
        if self.consecutive_denials != 0 {
            self.consecutive_denials = 0;
        }
    }

    /// Whether the breaker is tripped — 1:1 with claude-code `sel`:
    /// `consecutiveDenials >= maxConsecutive || totalDenials >= maxTotal`.
    #[must_use]
    pub const fn is_circuit_broken(&self) -> bool {
        self.consecutive_denials >= limits::MAX_CONSECUTIVE
            || self.total_denials >= limits::MAX_TOTAL
    }

    /// Evaluate the breaker after recording a deny — the structured port of
    /// claude-code `dSm`'s decision logic.
    ///
    /// Returns `None` when the breaker is NOT tripped (claude-code `if(!sel(e))
    /// return null`). When tripped, returns the [`DenialBreakerTrip`] the
    /// consumer must act on (emit telemetry, then abort-if-headless or
    /// warn-and-fall-back, then rewrite the decision reason). `headless` is the
    /// caller-supplied `shouldAvoidPermissionPrompts` (derived at the call site,
    /// not stored in this state — byte-faithful to `Fr(s).shouldAvoidPermissionPrompts`).
    ///
    /// SIDE EFFECT (the total-limit case only): mirrors claude-code's
    /// ``if(...,i) uAt(s,{...e,totalDenials:0,consecutiveDenials:0})`` — when the
    /// **total** limit tripped (`i`), both counters are reset HERE so the session
    /// can continue under interactive prompting. The consecutive-only case does
    /// NOT reset (claude-code resets `consecutiveDenials` separately, on the next
    /// non-deny, via [`Self::record_non_deny`]).
    #[must_use]
    pub fn trip(&mut self, headless: bool) -> Option<DenialBreakerTrip> {
        if !self.is_circuit_broken() {
            return None;
        }
        // `i = totalDenials >= maxTotal` — total dominates when both fire.
        let total_tripped = self.total_denials >= limits::MAX_TOTAL;
        let consecutive = self.consecutive_denials;
        let total = self.total_denials;
        let (limit, message) = if total_tripped {
            (
                DenialLimit::Total,
                format!(
                    "{total} actions were blocked this session. Please review the \
                     transcript before continuing."
                ),
            )
        } else {
            (
                DenialLimit::Consecutive,
                format!(
                    "{consecutive} consecutive actions were blocked. Please review the \
                     transcript before continuing."
                ),
            )
        };
        // claude-code `dSm`: on the TOTAL case, reset both counters (`uAt`).
        if total_tripped {
            self.consecutive_denials = 0;
            self.total_denials = 0;
        }
        Some(DenialBreakerTrip {
            limit,
            headless,
            consecutive_denials: consecutive,
            total_denials: total,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_zeroed_and_not_broken() {
        let s = DenialTrackingState::new();
        assert_eq!(s, DenialTrackingState::default());
        assert_eq!(s.consecutive_denials, 0);
        assert_eq!(s.total_denials, 0);
        assert!(!s.is_circuit_broken());
    }

    #[test]
    fn record_auto_deny_increments_both_counters() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        assert_eq!(s.consecutive_denials, 1);
        assert_eq!(s.total_denials, 1);
        s.record_auto_deny();
        assert_eq!(s.consecutive_denials, 2);
        assert_eq!(s.total_denials, 2);
    }

    #[test]
    fn trips_at_consecutive_threshold_three() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        s.record_auto_deny();
        assert!(!s.is_circuit_broken(), "2 consecutive must not trip");
        s.record_auto_deny();
        assert!(
            s.is_circuit_broken(),
            "3 consecutive must trip (maxConsecutive=3)"
        );
        assert_eq!(limits::MAX_CONSECUTIVE, 3);
    }

    #[test]
    fn trips_at_total_threshold_twenty_without_three_consecutive() {
        // Interleave denies and non-denies so consecutive never reaches 3, but
        // total climbs to 20. Pattern: 2 denies, 1 allow → repeat.
        let mut s = DenialTrackingState::new();
        let mut total = 0;
        while total < 20 {
            s.record_auto_deny();
            total += 1;
            s.record_auto_deny();
            total += 1;
            assert!(
                s.consecutive_denials < 3,
                "consecutive must stay below 3 in this pattern"
            );
            if total < 20 {
                s.record_non_deny(); // allow resets consecutive, NOT total
            }
        }
        assert_eq!(s.total_denials, 20);
        assert!(s.consecutive_denials < 3);
        assert!(
            s.is_circuit_broken(),
            "total=20 must trip (maxTotal=20) even with consecutive<3"
        );
        assert_eq!(limits::MAX_TOTAL, 20);
    }

    #[test]
    fn record_non_deny_resets_consecutive_but_not_total() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        s.record_auto_deny();
        assert_eq!(s.consecutive_denials, 2);
        assert_eq!(s.total_denials, 2);
        s.record_non_deny();
        assert_eq!(
            s.consecutive_denials, 0,
            "non-deny resets consecutive (Y4t)"
        );
        assert_eq!(s.total_denials, 2, "non-deny must NOT touch total");
        assert!(!s.is_circuit_broken());
    }

    #[test]
    fn record_non_deny_is_noop_when_already_zero() {
        let mut s = DenialTrackingState::new();
        s.record_non_deny();
        assert_eq!(s, DenialTrackingState::new());
        // and after a total-only increment shape (synthetic), consecutive 0 stays 0
        s.total_denials = 5;
        s.record_non_deny();
        assert_eq!(s.consecutive_denials, 0);
        assert_eq!(s.total_denials, 5);
    }

    #[test]
    fn is_circuit_broken_reader_matches_sel() {
        let mut s = DenialTrackingState::new();
        assert!(!s.is_circuit_broken());
        s.consecutive_denials = limits::MAX_CONSECUTIVE; // exactly 3
        assert!(s.is_circuit_broken(), "sel uses >=, so == threshold trips");
        s = DenialTrackingState::new();
        s.total_denials = limits::MAX_TOTAL; // exactly 20
        assert!(s.is_circuit_broken());
    }

    #[test]
    fn trip_returns_none_when_not_broken() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        assert!(s.trip(false).is_none(), "below threshold must not trip");
        assert!(s.trip(true).is_none());
    }

    #[test]
    fn trip_consecutive_case_cli_message_and_tags_no_reset() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        s.record_auto_deny();
        s.record_auto_deny(); // consecutive=3, total=3 -> consecutive trips, total does NOT
        let trip = s.trip(false).expect("must trip at 3 consecutive");
        assert_eq!(trip.limit, DenialLimit::Consecutive);
        assert_eq!(trip.limit.telemetry_tag(), "consecutive");
        assert_eq!(trip.mode_tag(), "cli");
        assert!(!trip.headless);
        assert_eq!(trip.consecutive_denials, 3);
        assert_eq!(trip.total_denials, 3);
        assert_eq!(
            trip.message,
            "3 consecutive actions were blocked. Please review the transcript before continuing."
        );
        // Consecutive-only case does NOT reset in trip() (reset happens on next non-deny).
        assert_eq!(
            s.consecutive_denials, 3,
            "consecutive case must not reset counters in trip"
        );
        assert_eq!(s.total_denials, 3);
    }

    #[test]
    fn trip_total_case_resets_both_counters_and_uses_total_message() {
        let mut s = DenialTrackingState::new();
        s.total_denials = limits::MAX_TOTAL; // 20
        s.consecutive_denials = 1; // below 3 -> only total trips
        let trip = s.trip(false).expect("total=20 must trip");
        assert_eq!(trip.limit, DenialLimit::Total);
        assert_eq!(trip.limit.telemetry_tag(), "total");
        assert_eq!(trip.total_denials, 20);
        assert_eq!(trip.consecutive_denials, 1);
        assert_eq!(
            trip.message,
            "20 actions were blocked this session. Please review the transcript before continuing."
        );
        // Total case resets BOTH counters (claude-code uAt reset).
        assert_eq!(s.consecutive_denials, 0, "total case resets consecutive");
        assert_eq!(s.total_denials, 0, "total case resets total");
    }

    #[test]
    fn trip_total_dominates_when_both_tripped() {
        let mut s = DenialTrackingState::new();
        s.consecutive_denials = 5; // >= 3
        s.total_denials = 25; // >= 20
        let trip = s.trip(false).expect("both tripped");
        assert_eq!(
            trip.limit,
            DenialLimit::Total,
            "i=totalDenials>=maxTotal dominates"
        );
    }

    #[test]
    fn trip_headless_sets_headless_flag_and_abort_message() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        s.record_auto_deny();
        s.record_auto_deny();
        let trip = s.trip(true).expect("must trip");
        assert!(trip.headless);
        assert_eq!(trip.mode_tag(), "headless");
        assert_eq!(
            DenialBreakerTrip::HEADLESS_ABORT_MESSAGE,
            "Agent aborted: too many classifier denials in headless mode"
        );
    }

    #[test]
    fn fallback_warn_line_and_decision_reason_are_byte_exact() {
        let mut s = DenialTrackingState::new();
        s.record_auto_deny();
        s.record_auto_deny();
        s.record_auto_deny();
        let trip = s.trip(false).expect("must trip");
        assert_eq!(
            trip.fallback_warn_line(),
            "Classifier denial limit exceeded, falling back to prompting: 3 consecutive \
             actions were blocked. Please review the transcript before continuing."
        );
        assert_eq!(
            trip.decision_reason("Bash(rm -rf /)"),
            "3 consecutive actions were blocked. Please review the transcript before \
             continuing.\n\nLatest blocked action: Bash(rm -rf /)"
        );
    }

    #[test]
    fn event_name_is_byte_locked() {
        assert_eq!(DENIAL_LIMIT_EVENT, "tengu_auto_mode_denial_limit_exceeded");
    }
}
