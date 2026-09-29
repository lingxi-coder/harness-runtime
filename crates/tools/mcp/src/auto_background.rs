//! Auto-backgrounding threshold for long-running MCP tool calls.
//!
//! Port of claude-code 2.1.212 `getMcpAutoBackgroundMs` (`Wc_`). When a single
//! MCP `tools/call` runs longer than this many milliseconds, the caller moves
//! the in-flight call to the background as an `mcp_task` and returns control to
//! the model (see [`crate::mcp_tool`]).
//!
//! The threshold is opt-in and gated:
//!   * `0` (disabled) for the IDE transports `sse-ide` / `ws-ide` (`jc_`), whose
//!     calls must never be detached.
//!   * `0` when `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` is truthy (`pE()`).
//!   * `0` for a non-interactive session UNLESS `CLAUDE_AUTO_BACKGROUND_TASKS`
//!     is truthy.
//!   * an explicit `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` override, clamped to
//!     `[0, i32::MAX]` (`Math.min(Math.max(0,r),2147483647)`), wins next.
//!   * otherwise the default `120000` when the `tengu_mcp_auto_background`
//!     feature flag is on (its production default), else `0`.

/// IDE transports whose calls are never auto-backgrounded (`jc_`).
const IDE_TRANSPORTS: &[&str] = &["sse-ide", "ws-ide"];

/// Default threshold when the `tengu_mcp_auto_background` flag is enabled
/// (`Uc_ = 120000`).
pub const DEFAULT_MCP_AUTO_BACKGROUND_MS: i64 = 120_000;

/// Clamp ceiling — `qc_ = 2147483647` (`i32::MAX`).
const MAX_AUTO_BACKGROUND_MS: i64 = 2_147_483_647;

/// The `tengu_mcp_auto_background` feature-flag key (default `true` in prod).
const AUTO_BACKGROUND_FLAG: &str = "tengu_mcp_auto_background";

/// Resolve the auto-background threshold (ms) for an MCP tool call on a client
/// of the given `transport_type` (e.g. `"stdio"`, `"sse"`, `"sse-ide"`).
///
/// Mirrors `Wc_(e,{isNonInteractiveSession})`. Returns `0` when
/// auto-backgrounding is disabled for this call.
#[must_use]
pub fn get_mcp_auto_background_ms(transport_type: &str, is_non_interactive_session: bool) -> i64 {
    get_mcp_auto_background_ms_inner(
        transport_type,
        is_non_interactive_session,
        std::env::var("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS")
            .ok()
            .as_deref(),
        std::env::var("CLAUDE_AUTO_BACKGROUND_TASKS")
            .ok()
            .as_deref(),
        std::env::var("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS")
            .ok()
            .as_deref(),
        telemetry::flag_bool(AUTO_BACKGROUND_FLAG, true),
    )
}

/// Env/flag-free core of [`get_mcp_auto_background_ms`] — every ambient input is
/// an explicit argument so the gate logic is deterministically unit-testable.
fn get_mcp_auto_background_ms_inner(
    transport_type: &str,
    is_non_interactive_session: bool,
    disable_background_tasks: Option<&str>,
    auto_background_tasks: Option<&str>,
    mcp_auto_background_ms: Option<&str>,
    flag_on: bool,
) -> i64 {
    // `if(jc_.has(e?.type??""))return 0` — IDE transports never detach.
    if IDE_TRANSPORTS.contains(&transport_type) {
        return 0;
    }
    // `if(pE())return 0` — global background-task kill switch.
    if lingxi_core::host::env::is_env_truthy(disable_background_tasks) {
        return 0;
    }
    // `if(t&&!Z.CLAUDE_AUTO_BACKGROUND_TASKS)return 0` — non-interactive opt-in.
    if is_non_interactive_session && !lingxi_core::host::env::is_env_truthy(auto_background_tasks) {
        return 0;
    }
    // `let r=Z.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS; if(r!==void 0)return
    // Math.min(Math.max(0,r),qc_)` — explicit override, clamped to [0,i32::MAX].
    if let Some(raw) = mcp_auto_background_ms {
        if !raw.trim().is_empty() {
            let parsed = lingxi_core::host::env::parse_int_env(raw);
            if !parsed.is_nan() {
                let clamped = parsed.max(0.0).min(MAX_AUTO_BACKGROUND_MS as f64);
                return clamped as i64;
            }
        }
    }
    // `return Qe("tengu_mcp_auto_background",!0)?Uc_:0`.
    if flag_on {
        DEFAULT_MCP_AUTO_BACKGROUND_MS
    } else {
        0
    }
}

/// Build the byte-exact user-facing text returned when a long-running MCP
/// tool call is moved to the background (claude-code 2.1.212
/// `callMcpToolWithAutoBackground`'s final `{type:"text",text:…}`).
///
/// * `description` — the `mcp_task`'s description (`serverName/toolName`).
/// * `task_id` — the minted `mcp_task` id (`k…`).
/// * `elapsed_secs` — `Math.round((Date.now()-start)/1000)`, seconds since the
///   call began.
///
/// The apostrophe in `you'll` is a straight ASCII `'` (verified against the
/// oracle binary), not a typographic quote.
#[must_use]
pub fn background_message(description: &str, task_id: &str, elapsed_secs: u64) -> String {
    format!(
        "MCP tool \"{description}\" is still running after {elapsed_secs}s. It was moved to the \
         background as task {task_id} and keeps running; you'll receive a notification with the \
         result when it completes. You can keep working in the meantime. To stop it, use TaskStop \
         with task_id \"{task_id}\". Note: it does not survive exiting this session."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The background message is byte-exact against the oracle template.
    #[test]
    fn background_message_is_byte_exact() {
        let msg = background_message("git/status", "k1a2b3c4", 120);
        assert_eq!(
            msg,
            "MCP tool \"git/status\" is still running after 120s. It was moved to the background \
             as task k1a2b3c4 and keeps running; you'll receive a notification with the result \
             when it completes. You can keep working in the meantime. To stop it, use TaskStop \
             with task_id \"k1a2b3c4\". Note: it does not survive exiting this session."
        );
        // Straight ASCII apostrophe, not a typographic quote.
        assert!(msg.contains("you'll"));
        assert!(!msg.contains('\u{2019}'));
    }

    // Default: interactive session, flag on, no env overrides → 120000.
    #[test]
    fn default_is_120000_when_flag_on() {
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, None, true),
            120_000
        );
    }

    // Flag off with no override → disabled.
    #[test]
    fn flag_off_disables() {
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, None, false),
            0
        );
    }

    // IDE transports are always excluded, even with the flag on and an override.
    #[test]
    fn ide_transports_excluded() {
        for t in ["sse-ide", "ws-ide"] {
            assert_eq!(
                get_mcp_auto_background_ms_inner(t, false, None, None, Some("5000"), true),
                0,
                "{t} should be excluded"
            );
        }
        // Non-IDE sibling transports are NOT excluded.
        assert_eq!(
            get_mcp_auto_background_ms_inner("sse", false, None, None, None, true),
            120_000
        );
    }

    // CLAUDE_CODE_DISABLE_BACKGROUND_TASKS truthy → disabled regardless.
    #[test]
    fn disable_background_tasks_kill_switch() {
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, Some("1"), None, Some("5000"), true),
            0
        );
        // A defined-but-falsy value does NOT disable.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, Some("0"), None, None, true),
            120_000
        );
    }

    // Non-interactive session is gated off unless CLAUDE_AUTO_BACKGROUND_TASKS.
    #[test]
    fn non_interactive_gate() {
        // Non-interactive, no opt-in → 0 even with flag on.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", true, None, None, None, true),
            0
        );
        // Non-interactive WITH opt-in → default applies.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", true, None, Some("true"), None, true),
            120_000
        );
        // Interactive is never gated by CLAUDE_AUTO_BACKGROUND_TASKS.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, None, true),
            120_000
        );
    }

    // Explicit override wins over the flag default and clamps to [0, i32::MAX].
    #[test]
    fn env_override_clamped() {
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("5000"), true),
            5_000
        );
        // Zero override is honored (an explicit disable), not treated as unset.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("0"), true),
            0
        );
        // Negative clamps up to 0.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("-1"), true),
            0
        );
        // Over-large clamps down to i32::MAX.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("9999999999"), true),
            2_147_483_647
        );
        // Override applies even when the flag is OFF.
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("3000"), false),
            3_000
        );
    }

    // A blank / unparseable override falls through to the flag default.
    #[test]
    fn blank_override_falls_through() {
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("   "), true),
            120_000
        );
        assert_eq!(
            get_mcp_auto_background_ms_inner("stdio", false, None, None, Some("abc"), true),
            120_000
        );
    }
}
