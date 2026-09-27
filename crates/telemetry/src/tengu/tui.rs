//! `tengu_tui_*` lifecycle events emitted by `lingxi-tui::run_tui_session`.
//!
//! M6-01 ships 4 events. M6-03 adds `streaming_render_{started,ended}`;
//! M6-05 adds `permission_dialog_{shown,resolved}`. Full inventory locked
//! at M6-09 release time.

/// Fired by `run_tui_session` after `RawGuard::enter()` succeeds and
/// before the first render. Payload: `session_id`, `cols`, `rows`.
pub const SESSION_STARTED: &str = "tengu_tui_session_started";

/// Fired by `run_tui_session` just before `RawGuard::exit()` runs,
/// regardless of exit reason (cancel / quit / error). Payload:
/// `session_id`, `duration_ms`, `ended_via` (one of
/// `"quit_key" / "cancelled" / "error"`).
pub const SESSION_ENDED: &str = "tengu_tui_session_ended";

/// Fired once per session, immediately after iocraft completes the first
/// reconcile (i.e. the user sees pixels). Payload: `session_id`,
/// `latency_ms` (since `SESSION_STARTED`).
pub const FIRST_RENDER: &str = "tengu_tui_first_render";

/// Fired on every `crossterm::event::Event::Resize`. Payload:
/// `session_id`, `cols`, `rows`.
pub const RESIZE: &str = "tengu_tui_resize";

// M6-03 additions: streaming render lifecycle.

/// Emitted by the render loop on the first render after the bridge
/// signals `TurnEvent::TurnStarted` (i.e. `state.streaming` transitions
/// `None → Some`). Payload: `session_id`. (M6-03)
pub const STREAMING_RENDER_STARTED: &str = "tengu_tui_streaming_render_started";

/// Emitted by the render loop on the first render after the bridge
/// signals `TurnEvent::TurnEnded(_)` (i.e. `state.streaming` transitions
/// `Some → None`). Payload: `session_id`, `outcome`. (M6-03)
pub const STREAMING_RENDER_ENDED: &str = "tengu_tui_streaming_render_ended";

// M6-05 additions: permission dialog lifecycle.

/// Emitted when a permission dialog transitions from `None` to `Some(_)`
/// in the TUI app state. Payload: `kind` (one of `"tool_use"`,
/// `"exit_plan_mode"`, `"bypass_permissions"`). (M6-05)
pub const PERMISSION_DIALOG_SHOWN: &str = "tengu_tui_permission_dialog_shown";

/// Emitted when the user resolves a permission dialog. Payload: `kind`,
/// `decision` (one of `"allow_once"`, `"allow_always"`, `"deny"`),
/// `persist` (bool), `elapsed_ms` (u64). (M6-05)
pub const PERMISSION_DIALOG_RESOLVED: &str = "tengu_tui_permission_dialog_resolved";

// M6-09 additions: scrollback scroll-mode lifecycle.

/// Emitted when the scrollback enters scroll mode (`scroll_offset` transitions
/// `0 → non-zero` — the user scrolled up away from the bottom). Payload:
/// `session_id`, `offset` (`Verified<usize>`). Emit site:
/// `lingxi-tui::app::scroll_with_viewport`. (M6-09)
pub const SCROLL_STARTED: &str = "tengu_tui_scroll_started";

/// Emitted when the scrollback exits scroll mode (`scroll_offset` transitions
/// `non-zero → 0` — the user returned to the bottom). Payload: `session_id`.
/// Emit site: `lingxi-tui::app::scroll_with_viewport`. (M6-09)
pub const SCROLL_ENDED: &str = "tengu_tui_scroll_ended";

// M7-16 additions: screen + message-search lifecycle (only those with real
// emit sites — see the M7-16 plan T0 audit). The screen open/close events fire
// on `AppState.active_screen` `None ↔ Some(_)` transitions; `search_opened`
// fires when the MessageSelector overlay opens. The remaining §2.7 candidates
// (`command_palette_opened`, `vim_mode_entered`, `key_pressed`) are DEFERRED —
// see the NOTE below.

/// Emitted when `AppState.active_screen` transitions `None → Some(_)` — a
/// full-page screen opens. Payload: `screen` (one of `"doctor"`, `"resume"`,
/// `"settings"`, `"memory"`, `"theme"`). Emit site:
/// `lingxi-tui::telemetry::screen_opened`, called from `AppState::open_doctor`
/// / `open_settings` / `open_memory` / `open_theme_picker` and the Resume open
/// path. (M7-16)
pub const SCREEN_OPENED: &str = "tengu_tui_screen_opened";

/// Emitted when `AppState.active_screen` transitions `Some(_) → None` — the
/// active screen closes back to the REPL. Payload: none (the screen kind is no
/// longer known once cleared). Emit site:
/// `lingxi-tui::telemetry::screen_closed`, called from `AppState::close_screen`
/// guarded so it fires only when a screen was actually open. (M7-16)
pub const SCREEN_CLOSED: &str = "tengu_tui_screen_closed";

/// Emitted when the `MessageSelector` search/jump/export overlay opens (Ctrl-T or
/// `/export`). Payload: `mode` (one of `"search"`, `"export"`). Emit site:
/// `lingxi-tui::telemetry::search_opened`, called from
/// `MessageSelectorState::open` / `open_export`. (M7-16)
pub const SEARCH_OPENED: &str = "tengu_tui_search_opened";

// NOTE (M6-09, carried + extended at M7-16): three §2.7 TUI candidates remain
// DEFERRED — each lacks a clean/aggregated emit site, so registering them would
// mint dead names (the M6 "330-vs-326, every registered name has a call site"
// lesson):
//   - `tengu_tui_key_pressed`: still no windowed `KeyPressedAggregator` (a
//     timer-flushed counter wired into the iocraft event loop). A raw per-key
//     emit violates the aggregation contract. Carried to M8.
//   - `tengu_tui_command_palette_opened`: `PaletteState::sync_from_prompt`
//     flips `open` false↔true on EVERY `/`-prefixed keystroke (and back on
//     Backspace), so there is no single once-per-open transition to hook — a
//     clean emit needs edge-detection across the whole `resync_overlays` flow.
//     Deferred to M8.
//   - `tengu_tui_vim_mode_entered`: Normal mode is (re)entered on the
//     Ctrl-Alt-V toggle AND on every Esc-from-Insert; the spec wants an
//     AGGREGATED entry, not per-Esc churn, and no aggregator exists. Deferred
//     to M8.

/// Order-locked array; appended into `tengu::ALL_EVENT_NAMES`. Append-only.
pub(crate) const NAMES: &[&str] = &[
    SESSION_STARTED,
    SESSION_ENDED,
    FIRST_RENDER,
    RESIZE,
    STREAMING_RENDER_STARTED,
    STREAMING_RENDER_ENDED,
    PERMISSION_DIALOG_SHOWN,
    PERMISSION_DIALOG_RESOLVED,
    SCROLL_STARTED,
    SCROLL_ENDED,
    SCREEN_OPENED,
    SCREEN_CLOSED,
    SEARCH_OPENED,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_has_13_entries_after_m7_16() {
        // 4 (M6-01) + 2 (M6-03 streaming render) + 2 (M6-05 permission
        // dialog shown/resolved) + 2 (M6-09 scroll started/ended) + 3 (M7-16:
        // screen_opened / screen_closed / search_opened — the candidates with
        // real emit sites) = 13. command_palette_opened / vim_mode_entered /
        // key_pressed stay deferred to M8 (no clean/aggregated emit site — see
        // module note).
        assert_eq!(NAMES.len(), 13);
    }

    #[test]
    fn event_name_strings_are_locked() {
        assert_eq!(SESSION_STARTED, "tengu_tui_session_started");
        assert_eq!(SESSION_ENDED, "tengu_tui_session_ended");
        assert_eq!(FIRST_RENDER, "tengu_tui_first_render");
        assert_eq!(RESIZE, "tengu_tui_resize");
        assert_eq!(
            STREAMING_RENDER_STARTED,
            "tengu_tui_streaming_render_started"
        );
        assert_eq!(STREAMING_RENDER_ENDED, "tengu_tui_streaming_render_ended");
        assert_eq!(PERMISSION_DIALOG_SHOWN, "tengu_tui_permission_dialog_shown");
        assert_eq!(
            PERMISSION_DIALOG_RESOLVED,
            "tengu_tui_permission_dialog_resolved"
        );
        assert_eq!(SCROLL_STARTED, "tengu_tui_scroll_started");
        assert_eq!(SCROLL_ENDED, "tengu_tui_scroll_ended");
        assert_eq!(SCREEN_OPENED, "tengu_tui_screen_opened");
        assert_eq!(SCREEN_CLOSED, "tengu_tui_screen_closed");
        assert_eq!(SEARCH_OPENED, "tengu_tui_search_opened");
    }

    #[test]
    fn m6_09_appends_scroll_events_before_m7_16_block() {
        // The M6-09 scroll pair sits immediately before the M7-16 block.
        let scroll: &[&str] = &NAMES[NAMES.len() - 5..NAMES.len() - 3];
        assert_eq!(scroll, &[SCROLL_STARTED, SCROLL_ENDED]);
    }

    #[test]
    fn m7_16_appends_screen_and_search_events_at_end() {
        // Append-only: the three M7-16 events are the tail, in registration
        // order.
        let last3: &[&str] = &NAMES[NAMES.len() - 3..];
        assert_eq!(last3, &[SCREEN_OPENED, SCREEN_CLOSED, SEARCH_OPENED]);
    }
}
