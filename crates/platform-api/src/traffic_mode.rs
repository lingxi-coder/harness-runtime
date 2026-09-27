//! TS-faithful traffic-mode / privacy kernel — port of claude-code's `M7a()` /
//! `ha()` / `F$e()` gate (CC 2.1.207).
//!
//! CC decides whether the CLI restricts non-essential network traffic and/or
//! telemetry from three env vars:
//!
//! ```js
//! function M7a(){
//!   if(process.env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) return "essential-traffic";
//!   if(process.env.DISABLE_TELEMETRY)                        return "no-telemetry";
//!   if(ct(process.env.DO_NOT_TRACK))                          return "no-telemetry";
//!   return "default";
//! }
//! function ha(){  return M7a()==="essential-traffic"; }   // essential-only
//! function F$e(){ return M7a()!=="default"; }             // telemetry disabled
//! function Gvn(){                                          // disabling var name
//!   if(process.env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) return "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
//!   if(process.env.DISABLE_TELEMETRY)                        return "DISABLE_TELEMETRY";
//!   if(ct(process.env.DO_NOT_TRACK))                          return "DO_NOT_TRACK";
//!   return null;
//! }
//! ```
//!
//! Byte-parity subtleties:
//!
//! * The first two vars use **plain JS truthiness** — ANY non-empty string
//!   (even `"0"`) disables; only unset or `""` is falsy. Mirrored here by
//!   [`env_present`] (present && non-empty), NOT [`crate::env::is_env_truthy`].
//! * `DO_NOT_TRACK` goes through CC's `ct()`, whose body is exactly the
//!   lowercase+trim+in-set{`1`,`true`,`yes`,`on`} predicate of
//!   [`crate::env::is_env_truthy`].
//!
//! Naming: LingXi renames `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` to
//! `LINGXI_DISABLE_NONESSENTIAL_TRAFFIC` (the workspace `CLAUDE_CODE_`→`LINGXI_`
//! convention) while still honoring the original spelling as a fallback — the
//! same dual-read pattern as the fast-mode gate in `tui::chat_widget`. The bare
//! vars (`DISABLE_TELEMETRY`, `DO_NOT_TRACK`, `DISABLE_ERROR_REPORTING`) keep
//! their CC names, matching how the repo already consumes `DISABLE_PROMPT_CACHING`
//! / `DISABLE_AUTOUPDATER` verbatim.
//!
//! Deferred: `DISABLE_COST_WARNINGS` (CC `_Dn()`) has exactly two call sites —
//! the exit-time `Cost:` stdout line and the `$5` `tengu_cost_threshold_reached`
//! UI warning — neither surface exists in LingXi yet, so its gate is deferred to
//! whenever that cost-warning surface is ported (do not invent a consumer here).
//! `DISABLE_NON_ESSENTIAL_MODEL_CALLS` is intentionally absent: it has zero hits
//! in the 2.1.207 binary (removed from CC); porting it would be anti-parity.

use crate::env::is_env_truthy;

/// Result of the traffic-mode kernel ([`traffic_mode`]) — the three return
/// values of CC's `M7a()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficMode {
    /// `"default"` — no privacy restriction.
    Default,
    /// `"no-telemetry"` — telemetry silenced (`DISABLE_TELEMETRY` / `DO_NOT_TRACK`).
    NoTelemetry,
    /// `"essential-traffic"` — only essential network traffic permitted
    /// (`LINGXI_DISABLE_NONESSENTIAL_TRAFFIC`).
    EssentialTraffic,
}

/// JS `process.env.X` truthiness: the var is present and non-empty. ANY
/// non-empty string (including `"0"` or whitespace) is truthy; only unset or
/// `""` is falsy. Distinct from [`is_env_truthy`], which additionally requires
/// membership in the `{1,true,yes,on}` set.
fn env_present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// True when the non-essential-traffic switch is set under either the LingXi
/// spelling (primary) or the original CC spelling (compat fallback).
fn nonessential_traffic_disabled() -> bool {
    env_present("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC")
        || env_present("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
}

/// `ct(process.env.DO_NOT_TRACK)` — strict `is_env_truthy` parse.
fn do_not_track() -> bool {
    let raw = std::env::var("DO_NOT_TRACK").ok();
    is_env_truthy(raw.as_deref())
}

/// Port of CC `M7a()` — the traffic-mode kernel. Precedence (first match wins):
/// non-essential-traffic ⇒ [`TrafficMode::EssentialTraffic`], then
/// `DISABLE_TELEMETRY` ⇒ [`TrafficMode::NoTelemetry`], then `DO_NOT_TRACK` ⇒
/// [`TrafficMode::NoTelemetry`], else [`TrafficMode::Default`].
#[must_use]
pub fn traffic_mode() -> TrafficMode {
    if nonessential_traffic_disabled() {
        return TrafficMode::EssentialTraffic;
    }
    if env_present("DISABLE_TELEMETRY") {
        return TrafficMode::NoTelemetry;
    }
    if do_not_track() {
        return TrafficMode::NoTelemetry;
    }
    TrafficMode::Default
}

/// Port of CC `ha()` — true when only essential network traffic is permitted.
#[must_use]
pub fn is_essential_traffic_only() -> bool {
    traffic_mode() == TrafficMode::EssentialTraffic
}

/// Port of CC `F$e()` — true when telemetry is disabled (any non-`Default`
/// mode). This is the gate the init-frame `analytics_disabled` flag and the
/// [`crate`]-wide telemetry sinks read.
#[must_use]
pub fn is_telemetry_disabled() -> bool {
    traffic_mode() != TrafficMode::Default
}

/// Port of CC `Gvn()` — the NAME of the env var that disabled telemetry, or
/// `None`. CC surfaces this in user-facing "disabled via `<VAR>`" messages;
/// LingXi reports the canonical LingXi spelling for the renamed
/// non-essential-traffic var and the verbatim CC names for the bare vars.
#[must_use]
pub fn telemetry_disabled_reason() -> Option<&'static str> {
    if nonessential_traffic_disabled() {
        return Some("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC");
    }
    if env_present("DISABLE_TELEMETRY") {
        return Some("DISABLE_TELEMETRY");
    }
    if do_not_track() {
        return Some("DO_NOT_TRACK");
    }
    None
}

/// Env portion of CC `lCu()` — the two `DISABLE_ERROR_REPORTING` / `F$e()`
/// gates that decide whether crash/error reports may be sent. CC additionally
/// requires first-party auth and a version check before actually reporting;
/// those checks — and the Sentry-style error-report egress itself — do not
/// exist in LingXi yet, so this helper lands the env gate now so that the
/// future error-report path inherits it. Returns `true` only when reporting is
/// permitted by the env layer.
#[must_use]
pub fn should_report_errors() -> bool {
    if env_present("DISABLE_ERROR_REPORTING") {
        return false;
    }
    if is_telemetry_disabled() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The traffic-mode vars are process-global; serialize every case that
    /// mutates them (Cargo shares the process env across parallel test threads).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// All five inputs cleared — the baseline `Default` state each case starts
    /// from so leakage from a sibling case can't mask a bug.
    fn clear_all() {
        for v in [
            "LINGXI_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "DISABLE_TELEMETRY",
            "DO_NOT_TRACK",
            "DISABLE_ERROR_REPORTING",
        ] {
            std::env::remove_var(v);
        }
    }

    #[test]
    fn default_when_all_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all();
        assert_eq!(traffic_mode(), TrafficMode::Default);
        assert!(!is_essential_traffic_only());
        assert!(!is_telemetry_disabled());
        assert_eq!(telemetry_disabled_reason(), None);
        assert!(should_report_errors());
        clear_all();
    }

    #[test]
    fn nonessential_traffic_wins_and_uses_plain_truthiness() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all();
        // Plain JS truthiness: even "0" is truthy (non-empty string).
        std::env::set_var("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC", "0");
        assert_eq!(traffic_mode(), TrafficMode::EssentialTraffic);
        assert!(is_essential_traffic_only());
        assert!(is_telemetry_disabled());
        assert_eq!(
            telemetry_disabled_reason(),
            Some("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC")
        );
        assert!(!should_report_errors());
        clear_all();

        // Empty string is falsy (JS `""` ⇒ falsy) ⇒ back to Default.
        std::env::set_var("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC", "");
        assert_eq!(traffic_mode(), TrafficMode::Default);
        clear_all();

        // Original CC spelling is honored as a compat fallback.
        std::env::set_var("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        assert_eq!(traffic_mode(), TrafficMode::EssentialTraffic);
        clear_all();
    }

    #[test]
    fn nonessential_traffic_beats_telemetry_and_do_not_track() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all();
        std::env::set_var("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        std::env::set_var("DISABLE_TELEMETRY", "1");
        std::env::set_var("DO_NOT_TRACK", "1");
        // First match in M7a order wins.
        assert_eq!(traffic_mode(), TrafficMode::EssentialTraffic);
        assert_eq!(
            telemetry_disabled_reason(),
            Some("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC")
        );
        clear_all();
    }

    #[test]
    fn disable_telemetry_plain_truthiness_beats_do_not_track() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all();
        // "0" is a non-empty string ⇒ JS-truthy ⇒ NoTelemetry.
        std::env::set_var("DISABLE_TELEMETRY", "0");
        assert_eq!(traffic_mode(), TrafficMode::NoTelemetry);
        assert!(!is_essential_traffic_only());
        assert!(is_telemetry_disabled());
        assert_eq!(telemetry_disabled_reason(), Some("DISABLE_TELEMETRY"));
        clear_all();

        // Empty ⇒ falsy ⇒ Default.
        std::env::set_var("DISABLE_TELEMETRY", "");
        assert_eq!(traffic_mode(), TrafficMode::Default);
        clear_all();
    }

    #[test]
    fn do_not_track_uses_strict_is_env_truthy_parse() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all();
        // Strict ct(): "1"/"true"/"yes"/"on" ⇒ NoTelemetry.
        for v in ["1", "true", "YES", " on "] {
            std::env::set_var("DO_NOT_TRACK", v);
            assert_eq!(
                traffic_mode(),
                TrafficMode::NoTelemetry,
                "DO_NOT_TRACK={v:?} should disable telemetry"
            );
            assert_eq!(telemetry_disabled_reason(), Some("DO_NOT_TRACK"));
        }
        clear_all();

        // Strict ct(): "0"/out-of-set/empty ⇒ Default (unlike DISABLE_TELEMETRY).
        for v in ["0", "false", "2", ""] {
            std::env::set_var("DO_NOT_TRACK", v);
            assert_eq!(
                traffic_mode(),
                TrafficMode::Default,
                "DO_NOT_TRACK={v:?} should NOT disable telemetry"
            );
        }
        clear_all();
    }

    #[test]
    fn error_reporting_gate() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all();
        assert!(should_report_errors(), "permitted when env is clean");

        std::env::set_var("DISABLE_ERROR_REPORTING", "1");
        assert!(!should_report_errors(), "DISABLE_ERROR_REPORTING blocks");
        clear_all();

        // Telemetry-disabled also blocks error reporting (lCu's second gate).
        std::env::set_var("DO_NOT_TRACK", "1");
        assert!(!should_report_errors(), "F$e()==true blocks");
        clear_all();
    }
}
