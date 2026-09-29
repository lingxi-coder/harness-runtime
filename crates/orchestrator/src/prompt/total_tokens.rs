//! `total_tokens_reminder` — the `<total_tokens>N tokens left</total_tokens>`
//! budget block the oracle emits after every tool-result batch (and, when
//! `totalTokensReminderAfterUserTurn` is on, after every regular user prompt).
//!
//! Oracle anatomy (offsets into `~/.local/share/claude/versions/2.1.238`;
//! identical machinery already existed in 2.1.220 as `Qdo` @230540538, so this
//! is a PRE-EXISTING port gap rather than 238 drift):
//!
//! * formatter `dOi(mode, remaining)` @ **292022017**:
//!   ```js
//!   `<total_tokens>${e==="infinite"?"Infinite":e==="fixed"?zBv:Math.max(0,t)} tokens left</total_tokens>`
//!   ```
//!   with `zBv = 5000000` and the padded budget `X3f = 15000000`.
//! * mode resolution `srt()`/`qBv()` @ **292020823**:
//!   `CLAUDE_CODE_TOTAL_TOKENS_REMINDER` → settings `totalTokensReminder` →
//!   client data → GrowthBook `tengu_lapis_anchor`, whose binary literal
//!   fallback is **`"padded-countdown"`** (i.e. ON).
//! * budget `uOi()`/`WBv()`: `CLAUDE_CODE_TOTAL_TOKENS_REMINDER_BUDGET` →
//!   settings `totalTokensReminderBudget` → client data → `X3f`.
//! * after-user-turn `Q3f()`/`GBv()`:
//!   `CLAUDE_CODE_TOTAL_TOKENS_REMINDER_AFTER_USER_TURN` → settings →
//!   client data → GrowthBook `tengu_lapis_anchor_user_turn`, default `true`.
//! * producer `D3T(session, msgs, model, agentId, reanchor)` @ **296556375**:
//!   ```js
//!   let i=srt(); if(i==="off")return[];
//!   let s=n??"main", a=hoe(t), l=RYn.of(e);
//!   if(o) l.reanchorTaskBudget(s,a);
//!   let c = i==="countdown"      ? OR(r,Ox())-a
//!         : i==="padded-countdown" ? uOi()-l.cumulativeUsed(s,a)
//!         : 0;
//!   return [{type:"total_tokens_reminder", text:dOi(i,c)}]
//!   ```
//! * renderer @ **296738663**:
//!   `total_tokens_reminder:(e)=>[kn({content:NT(e.text),isMeta:!0})]` — the
//!   `<system-reminder>\n…\n</system-reminder>` envelope on a meta user
//!   message, applied by the injection site in `conversation.rs`.
//!
//! `hoe(messages)` @ **294688350** is the LAST assistant message's usage
//! (`input + cache_creation + cache_read + output`), not an estimate.
//!
//! **DEFAULT ON, matching the oracle (2026-08-20).** This shipped one pass
//! earlier defaulting to [`TotalTokensMode::Off`], on the reasoning that
//! flipping it would churn every turn's outgoing message list and the locked
//! fixtures. That deferral is now paid off: [`PORT_DEFAULT_MODE`] is
//! [`ORACLE_DEFAULT_MODE`], so a stock LingXi session emits this reminder on
//! nearly every model step exactly as a stock Claude Code session does.
//!
//! The oracle default was confirmed LIVE rather than inferred from the
//! GrowthBook fallback literal: a real 2.1.238 session emits
//! `<total_tokens>14999028 tokens left</total_tokens>`, i.e. `padded-countdown`
//! counting down from [`DEFAULT_TOTAL_TOKENS_BUDGET`] (15_000_000).
//!
//! To turn it off for a session, set `CLAUDE_CODE_TOTAL_TOKENS_REMINDER=off`
//! or the `totalTokensReminder` setting — the same two tiers the oracle reads.

use std::collections::HashMap;

/// `zBv` @292022017 — the literal reported by the `fixed` mode.
pub const FIXED_TOTAL_TOKENS: u64 = 5_000_000;

/// `X3f` @292022017 — the default `padded-countdown` budget.
pub const DEFAULT_TOTAL_TOKENS_BUDGET: u64 = 15_000_000;

/// `jBv` @292022017 — the accepted mode strings, in the oracle's order.
pub const TOTAL_TOKENS_MODES: &[&str] =
    &["off", "infinite", "fixed", "countdown", "padded-countdown"];

/// The oracle's GrowthBook literal fallback for `tengu_lapis_anchor`.
pub const ORACLE_DEFAULT_MODE: TotalTokensMode = TotalTokensMode::PaddedCountdown;

/// LingXi's default. Now the SAME as [`ORACLE_DEFAULT_MODE`]: the reminder is
/// on by default, as in a stock Claude Code session.
///
/// Confirmed live rather than inferred — a real 2.1.238 session emits
/// `<total_tokens>14999028 tokens left</total_tokens>` against the 15_000_000
/// [`DEFAULT_TOTAL_TOKENS_BUDGET`], i.e. `padded-countdown`.
pub const PORT_DEFAULT_MODE: TotalTokensMode = ORACLE_DEFAULT_MODE;

/// `srt()`'s result type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotalTokensMode {
    /// No reminder at all.
    Off,
    /// Always reports the literal `Infinite`.
    Infinite,
    /// Always reports [`FIXED_TOTAL_TOKENS`].
    Fixed,
    /// Reports the live remaining context-window tokens.
    Countdown,
    /// Counts down from [`resolve_budget`], re-anchored on each regular user
    /// prompt when [`after_user_turn`] is on.
    PaddedCountdown,
}

impl TotalTokensMode {
    /// `cOi(e)` — parse one of [`TOTAL_TOKENS_MODES`]; anything else is `None`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "infinite" => Some(Self::Infinite),
            "fixed" => Some(Self::Fixed),
            "countdown" => Some(Self::Countdown),
            "padded-countdown" => Some(Self::PaddedCountdown),
            _ => None,
        }
    }

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Infinite => "infinite",
            Self::Fixed => "fixed",
            Self::Countdown => "countdown",
            Self::PaddedCountdown => "padded-countdown",
        }
    }
}

/// `qBv()` — resolve the active mode.
///
/// `CLAUDE_CODE_TOTAL_TOKENS_REMINDER` wins; `settings_mode` is the caller's
/// resolved `totalTokensReminder` setting (the oracle's second tier); the
/// client-data / GrowthBook tiers are unobservable from the binary, so the
/// chain ends at [`PORT_DEFAULT_MODE`].
#[must_use]
pub fn resolve_mode(settings_mode: Option<&str>) -> TotalTokensMode {
    if let Some(mode) = std::env::var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER")
        .ok()
        .and_then(|raw| TotalTokensMode::parse(raw.trim()))
    {
        return mode;
    }
    if let Some(mode) = settings_mode.and_then(|raw| TotalTokensMode::parse(raw.trim())) {
        return mode;
    }
    PORT_DEFAULT_MODE
}

/// `WBv()` — resolve the `padded-countdown` budget.
#[must_use]
pub fn resolve_budget(settings_budget: Option<u64>) -> u64 {
    if let Some(v) = std::env::var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER_BUDGET")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
    {
        return v;
    }
    settings_budget
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TOTAL_TOKENS_BUDGET)
}

/// `Q3f()`/`GBv()` — re-anchor the padded budget on each regular user prompt?
/// Oracle default is `true`.
#[must_use]
pub fn after_user_turn(settings_flag: Option<bool>) -> bool {
    let raw = std::env::var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER_AFTER_USER_TURN").ok();
    if lingxi_core::host::env::is_env_truthy(raw.as_deref()) {
        return true;
    }
    if lingxi_core::host::env::is_env_defined_falsy(raw.as_deref()) {
        return false;
    }
    settings_flag.unwrap_or(true)
}

/// `dOi(mode, remaining)` — the byte-exact reminder body (no
/// `<system-reminder>` envelope; the injection site adds that).
#[must_use]
pub fn format_total_tokens(mode: TotalTokensMode, remaining: i64) -> String {
    let value = match mode {
        TotalTokensMode::Infinite => "Infinite".to_string(),
        TotalTokensMode::Fixed => FIXED_TOTAL_TOKENS.to_string(),
        _ => remaining.max(0).to_string(),
    };
    format!("<total_tokens>{value} tokens left</total_tokens>")
}

/// `Z3f` @292021?? — the per-agent padded-countdown ledger.
///
/// * `rollOverContext(key, dropped)` — compaction hands back the tokens it
///   dropped so the countdown keeps falling across a compact.
/// * `reanchorTaskBudget(key, used)` — a regular user prompt restarts the
///   budget from the full amount.
/// * `cumulativeUsed(key, used)` — monotonic (`Math.max` against the previous
///   high-water mark), so a shrinking context never makes the countdown rise.
#[derive(Debug, Default)]
pub struct TotalTokensLedger {
    rollovers: HashMap<String, i64>,
    anchors: HashMap<String, i64>,
    max_seen: HashMap<String, i64>,
}

impl TotalTokensLedger {
    /// `rollOverContext(e,t)` — accumulate tokens dropped by a compaction.
    pub fn roll_over_context(&mut self, key: &str, dropped: i64) {
        let slot = self.rollovers.entry(key.to_string()).or_insert(0);
        *slot += dropped;
    }

    /// `reanchorTaskBudget(e,t)` — restart the countdown from the full budget.
    pub fn reanchor_task_budget(&mut self, key: &str, used: i64) {
        let rolled = self.rollovers.get(key).copied().unwrap_or(0);
        self.anchors.insert(key.to_string(), rolled + used);
        self.max_seen.insert(key.to_string(), 0);
    }

    /// `cumulativeUsed(e,t)` — the monotonic used-token total for `key`.
    pub fn cumulative_used(&mut self, key: &str, used: i64) -> i64 {
        let rolled = self.rollovers.get(key).copied().unwrap_or(0);
        let anchor = self.anchors.get(key).copied().unwrap_or(0);
        let n = rolled + used - anchor;
        let slot = self.max_seen.entry(key.to_string()).or_insert(0);
        *slot = (*slot).max(n);
        *slot
    }
}

/// `D3T`'s arithmetic: the `remaining` value handed to [`format_total_tokens`].
///
/// `context_window` is the oracle's `OR(model, Ox())` (only read by
/// `countdown`); `used` is `hoe(messages)`.
#[must_use]
pub fn remaining_tokens(
    mode: TotalTokensMode,
    ledger: &mut TotalTokensLedger,
    agent_key: &str,
    used: i64,
    context_window: i64,
    budget: u64,
) -> i64 {
    match mode {
        TotalTokensMode::Countdown => context_window - used,
        TotalTokensMode::PaddedCountdown => {
            let budget = i64::try_from(budget).unwrap_or(i64::MAX);
            budget - ledger.cumulative_used(agent_key, used)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatter_is_byte_exact_against_2_1_238() {
        assert_eq!(
            format_total_tokens(TotalTokensMode::PaddedCountdown, 14_999_000),
            "<total_tokens>14999000 tokens left</total_tokens>"
        );
        assert_eq!(
            format_total_tokens(TotalTokensMode::Infinite, 0),
            "<total_tokens>Infinite tokens left</total_tokens>"
        );
        assert_eq!(
            format_total_tokens(TotalTokensMode::Fixed, 123),
            "<total_tokens>5000000 tokens left</total_tokens>"
        );
    }

    /// `Math.max(0, t)` — an overspent budget clamps at zero, never negative.
    #[test]
    fn a_negative_remaining_clamps_to_zero() {
        assert_eq!(
            format_total_tokens(TotalTokensMode::Countdown, -42),
            "<total_tokens>0 tokens left</total_tokens>"
        );
    }

    #[test]
    fn oracle_constants_match_the_binary() {
        assert_eq!(FIXED_TOTAL_TOKENS, 5_000_000);
        assert_eq!(DEFAULT_TOTAL_TOKENS_BUDGET, 15_000_000);
        assert_eq!(
            TOTAL_TOKENS_MODES.to_vec(),
            vec!["off", "infinite", "fixed", "countdown", "padded-countdown"]
        );
        assert_eq!(ORACLE_DEFAULT_MODE, TotalTokensMode::PaddedCountdown);
    }

    /// The port default now MATCHES the oracle. Asserted against
    /// `ORACLE_DEFAULT_MODE` rather than a repeated literal, so the two can
    /// never drift apart again the way they did while this was deferred.
    #[test]
    fn port_default_matches_the_oracle() {
        assert_eq!(PORT_DEFAULT_MODE, ORACLE_DEFAULT_MODE);
        assert_eq!(resolve_mode(None), TotalTokensMode::PaddedCountdown);
    }

    /// Turning it off is still reachable through the same two tiers the oracle
    /// reads, so a session that does not want the block can suppress it.
    #[test]
    fn settings_can_turn_the_reminder_off() {
        assert_eq!(resolve_mode(Some("off")), TotalTokensMode::Off);
    }

    #[test]
    fn settings_mode_overrides_the_port_default() {
        assert_eq!(
            resolve_mode(Some("padded-countdown")),
            TotalTokensMode::PaddedCountdown
        );
        // An unparseable value falls through, exactly like `cOi`'s guard — and
        // "falls through" now means the ORACLE default, not `Off`, since the
        // port default was aligned. A typo'd setting therefore leaves the
        // reminder ON, which is what a stock Claude Code does too.
        assert_eq!(
            resolve_mode(Some("nonsense")),
            TotalTokensMode::PaddedCountdown
        );
    }

    #[test]
    fn budget_defaults_and_settings_override() {
        assert_eq!(resolve_budget(None), 15_000_000);
        assert_eq!(resolve_budget(Some(0)), 15_000_000);
        assert_eq!(resolve_budget(Some(1_000)), 1_000);
    }

    #[test]
    fn cumulative_used_is_monotonic() {
        let mut ledger = TotalTokensLedger::default();
        assert_eq!(ledger.cumulative_used("main", 100), 100);
        assert_eq!(ledger.cumulative_used("main", 250), 250);
        // A compaction shrank the live context: the high-water mark holds.
        assert_eq!(ledger.cumulative_used("main", 40), 250);
    }

    #[test]
    fn rollover_keeps_the_countdown_falling_across_a_compact() {
        let mut ledger = TotalTokensLedger::default();
        assert_eq!(ledger.cumulative_used("main", 1_000), 1_000);
        ledger.roll_over_context("main", 900);
        assert_eq!(ledger.cumulative_used("main", 200), 1_100);
    }

    #[test]
    fn reanchor_restarts_the_padded_budget() {
        let mut ledger = TotalTokensLedger::default();
        assert_eq!(ledger.cumulative_used("main", 5_000), 5_000);
        ledger.reanchor_task_budget("main", 5_000);
        assert_eq!(ledger.cumulative_used("main", 5_000), 0);
        assert_eq!(ledger.cumulative_used("main", 5_400), 400);
    }

    #[test]
    fn padded_countdown_remaining_uses_the_budget() {
        let mut ledger = TotalTokensLedger::default();
        let left = remaining_tokens(
            TotalTokensMode::PaddedCountdown,
            &mut ledger,
            "main",
            1_000,
            200_000,
            DEFAULT_TOTAL_TOKENS_BUDGET,
        );
        assert_eq!(left, 14_999_000);
    }

    #[test]
    fn countdown_remaining_uses_the_context_window() {
        let mut ledger = TotalTokensLedger::default();
        let left = remaining_tokens(
            TotalTokensMode::Countdown,
            &mut ledger,
            "main",
            50_000,
            200_000,
            DEFAULT_TOTAL_TOKENS_BUDGET,
        );
        assert_eq!(left, 150_000);
    }
}
