//! The `TokenWarning` context-pressure banner: its byte-exact label + color.
//!
//! 1:1 port of the render branch of claude-code `components/TokenWarning.tsx`
//! (`:108` gate, `:113`/`:118` `showAutoCompactWarning`, `:166-169` labels).
//! The component is mounted in the input chrome by
//! `components/PromptInput/Notifications.tsx:321`
//! (`{!isBriefOnly && <TokenWarning tokenUsage model />}`). This module owns the
//! string + color LOGIC; the iocraft render + live-token-usage threading is the
//! TUI integration layer that consumes it.
//!
//! The `REACTIVE_COMPACT` (`tengu_cobalt_raccoon`) and `CONTEXT_COLLAPSE`
//! feature flags are OFF in this build, so `displayPercentLeft === percentLeft`,
//! `reactiveOnlyMode === false`, and `collapseMode === false` — the default
//! branch. When they land, the reactive `"{100 - N}% context used"` label and
//! the `CollapseLabel` sub-component (`TokenWarning.tsx:21-86`) extend here.

use crate::thresholds::TokenWarningState;

/// Render color for the banner — mirrors the `<Text>` color props at
/// `TokenWarning.tsx:169`: `dimColor` for the auto-compact countdown,
/// `color="error"` / `color="warning"` for the "Context low" line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenWarningColor {
    /// `dimColor={true}` — the `showAutoCompactWarning` branch.
    Dim,
    /// `color="warning"` — "Context low" below the error threshold.
    Warning,
    /// `color="error"` — "Context low" at/above the error threshold.
    Error,
}

/// The rendered banner: the exact text and its color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenWarningBanner {
    /// The byte-exact label text (no surrounding chrome).
    pub text: String,
    /// The text color.
    pub color: TokenWarningColor,
}

/// Build the `TokenWarning` banner, or `None` when it must not render.
///
/// 1:1 with `TokenWarning.tsx`:
/// - `if (!isAboveWarningThreshold || suppressWarning) return null` (`:108`).
/// - `showAutoCompactWarning = isAutoCompactEnabled()` (`:113`) — the caller
///   passes the resolved value (LingXi [`crate::thresholds::is_auto_compact_enabled`]).
/// - `suppressWarning = useCompactWarningSuppression()` (`:107`) — LingXi
///   [`crate::is_compact_warning_suppressed`].
/// - `upgradeMessage = getUpgradeMessage("warning")` (`:121`) — `None` when no
///   context-window upgrade is offered (the common case).
/// - labels (`:166-169`): the auto-compact countdown (`dimColor`) vs the
///   "Context low" line (`error`/`warning`), each gaining a `· {upgradeMessage}`
///   suffix when an upgrade is offered.
///
/// The `·` separator is U+00B7 (the reference's `·`), reproduced verbatim.
#[must_use]
/// Raw-truthy read of `DISABLE_COMPACT` (binary `je.DISABLE_COMPACT`): any
/// non-empty value disables the `/compact` command, so its banner CTA is dropped.
fn disable_compact_env() -> bool {
    std::env::var_os("DISABLE_COMPACT").is_some_and(|v| !v.is_empty())
}

/// Pure builder for the "Context low" line (binary @206545348 3-way ternary):
/// an upgrade CTA wins; else a set `DISABLE_COMPACT` drops the CTA; else the
/// `· Run /compact` CTA. Separated for env-race-free testing.
fn context_low_text(
    percent_left: u8,
    upgrade_message: Option<&str>,
    disable_compact: bool,
) -> String {
    match upgrade_message {
        Some(upgrade) => format!("Context low ({percent_left}% remaining) \u{00b7} {upgrade}"),
        None if disable_compact => format!("Context low ({percent_left}% remaining)"),
        None => format!(
            "Context low ({percent_left}% remaining) \u{00b7} Run /compact to compact & continue"
        ),
    }
}

pub fn token_warning_banner(
    state: &TokenWarningState,
    auto_compact_enabled: bool,
    suppress_warning: bool,
    upgrade_message: Option<&str>,
) -> Option<TokenWarningBanner> {
    // `:108` — only shown above the warning threshold, and never while a
    // post-compaction suppression window is active.
    if !state.is_above_warning_threshold || suppress_warning {
        return None;
    }
    let percent_left = state.percent_left;
    if auto_compact_enabled {
        // `:166` reactiveOnlyMode === false ⇒ `${displayPercentLeft}% until auto-compact`
        // (displayPercentLeft === percentLeft with REACTIVE_COMPACT off).
        let label = format!("{percent_left}% until auto-compact");
        let text = match upgrade_message {
            Some(upgrade) => format!("{label} \u{00b7} {upgrade}"),
            None => label,
        };
        Some(TokenWarningBanner {
            text,
            color: TokenWarningColor::Dim,
        })
    } else {
        // `:169` the warning/error "Context low" line — a 3-way ternary
        // (binary @206545348): an upgrade CTA wins; else when `DISABLE_COMPACT`
        // is set the bare line shows with NO `· Run /compact` CTA (the command is
        // disabled — `isEnabled:()=>!je.DISABLE_COMPACT`); else the CTA. The env
        // check is the binary's raw `je.DISABLE_COMPACT` (any non-empty value).
        let text = context_low_text(percent_left, upgrade_message, disable_compact_env());
        let color = if state.is_above_error_threshold {
            TokenWarningColor::Error
        } else {
            TokenWarningColor::Warning
        };
        Some(TokenWarningBanner { text, color })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `TokenWarningState` with the threshold flags under test; the
    /// non-relevant flags are left `false`.
    #[test]
    fn context_low_text_three_way() {
        // Upgrade CTA wins.
        assert_eq!(
            context_low_text(15, Some("Upgrade for 1M"), false),
            "Context low (15% remaining) \u{00b7} Upgrade for 1M"
        );
        // DISABLE_COMPACT set, no upgrade → bare line, NO CTA.
        assert_eq!(
            context_low_text(8, None, true),
            "Context low (8% remaining)"
        );
        // Normal: the "Run /compact" CTA.
        assert_eq!(
            context_low_text(8, None, false),
            "Context low (8% remaining) \u{00b7} Run /compact to compact & continue"
        );
    }

    fn state(percent_left: u8, above_warning: bool, above_error: bool) -> TokenWarningState {
        TokenWarningState {
            percent_left,
            is_above_warning_threshold: above_warning,
            is_above_error_threshold: above_error,
            is_above_auto_compact_threshold: false,
            is_at_blocking_limit: false,
        }
    }

    /// `token_warning_banner` reads `DISABLE_COMPACT` from the process env, and
    /// the threshold tests in this same binary SET it. Take the crate-wide
    /// [`ENV_LOCK`] and clear the variable for the duration, restoring it
    /// before the guard drops — a lock that is not taken by every participant
    /// serializes nothing, which is exactly how this raced.
    fn without_disable_compact<T>(body: impl FnOnce() -> T) -> T {
        let _guard = crate::thresholds::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var("DISABLE_COMPACT").ok();
        std::env::remove_var("DISABLE_COMPACT");
        let out = body();
        match saved {
            Some(value) => std::env::set_var("DISABLE_COMPACT", value),
            None => std::env::remove_var("DISABLE_COMPACT"),
        }
        out
    }

    #[test]
    fn below_warning_threshold_renders_nothing() {
        // `:108` — no banner until the warning threshold is crossed.
        assert_eq!(
            token_warning_banner(&state(40, false, false), true, false, None),
            None
        );
        assert_eq!(
            token_warning_banner(&state(40, false, false), false, false, None),
            None
        );
    }

    #[test]
    fn suppressed_renders_nothing_even_above_threshold() {
        // `useCompactWarningSuppression()` short-circuits the banner after a
        // successful compaction.
        assert_eq!(
            token_warning_banner(&state(10, true, true), false, true, None),
            None
        );
    }

    #[test]
    fn auto_compact_enabled_shows_until_auto_compact_countdown_dimmed() {
        // `:166`/`:169` — `${percentLeft}% until auto-compact`, dimColor.
        let banner =
            token_warning_banner(&state(12, true, false), true, false, None).expect("renders");
        assert_eq!(banner.text, "12% until auto-compact");
        assert_eq!(banner.color, TokenWarningColor::Dim);
    }

    #[test]
    fn auto_compact_disabled_shows_context_low_warning_byte_exact() {
        // `:169` warning branch — exact string incl. the U+00B7 separator and
        // the `Run /compact to compact & continue` CTA.
        let banner = without_disable_compact(|| {
            token_warning_banner(&state(8, true, false), false, false, None).expect("renders")
        });
        assert_eq!(
            banner.text,
            "Context low (8% remaining) \u{00b7} Run /compact to compact & continue"
        );
        assert_eq!(banner.color, TokenWarningColor::Warning);
        // Lock the separator glyph as U+00B7 (middle dot), not an ASCII hyphen
        // or a bullet — matching the reference `·`.
        assert!(banner.text.contains('\u{00b7}'));
        assert!(!banner.text.contains(" - "));
    }

    #[test]
    fn above_error_threshold_colors_context_low_as_error() {
        // `:169` `color={isAboveErrorThreshold ? "error" : "warning"}`.
        let banner = without_disable_compact(|| {
            token_warning_banner(&state(3, true, true), false, false, None).expect("renders")
        });
        assert_eq!(
            banner.text,
            "Context low (3% remaining) \u{00b7} Run /compact to compact & continue"
        );
        assert_eq!(banner.color, TokenWarningColor::Error);
    }

    #[test]
    fn upgrade_message_appended_with_middle_dot_on_both_branches() {
        // `:169` `${autocompactLabel} · ${upgradeMessage}` and
        // `Context low (...) · ${upgradeMessage}`.
        let auto =
            token_warning_banner(&state(15, true, false), true, false, Some("Upgrade for 1M"))
                .expect("renders");
        assert_eq!(auto.text, "15% until auto-compact \u{00b7} Upgrade for 1M");
        assert_eq!(auto.color, TokenWarningColor::Dim);

        let low = token_warning_banner(&state(5, true, true), false, false, Some("Upgrade for 1M"))
            .expect("renders");
        assert_eq!(
            low.text,
            "Context low (5% remaining) \u{00b7} Upgrade for 1M"
        );
        assert_eq!(low.color, TokenWarningColor::Error);
    }
}
