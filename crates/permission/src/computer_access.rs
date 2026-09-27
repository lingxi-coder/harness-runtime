//! Backend-neutral `computer` tool `request_access` prompt bridge types.
//!
//! Mirrors `tool_api::ask_user_question`: a pure-data description of
//! the request plus a one-shot reply channel the TUI fills when the user
//! resolves the widget. The `computer`-tool-owned `ComputerAccessResolver`
//! (in `tool-computer-use`) constructs a [`ComputerAccessExchange`], sends it
//! to the TUI app, and awaits the response on `resp_tx` — the same round-trip
//! `tool_api::ask_user_question::AskUserQuestionExchange` performs, chosen over
//! reusing the GENERIC `permission_bridge::PermissionExchange` because a
//! title+message+options-list prompt can't express per-app checkboxes, a
//! tier, three independent capability flags, or a TCC missing-permissions
//! panel — parity target: claude-code's `ComputerUseApproval.tsx`.
//!
//! Backend-neutral so both the `tool-computer-use` producer and the `tui`
//! renderer can share these shapes without either depending on the other.

use tokio::sync::oneshot;

/// The per-app capability level `request_access` can grant (parity with the
/// binary's `read`/`click`/`full` tiers — see
/// `tool_computer_use::permission_model::AppTier`, which this mirrors at the
/// render boundary so `tui-core` doesn't need to depend on the tool crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessTier {
    /// Visible in screenshots only — no clicks or typing.
    Read,
    /// Plain clicks, scroll, drag, cursor queries.
    Click,
    /// Full interaction: right-click, modifier-clicks, typing, key presses.
    Full,
}

impl AccessTier {
    /// Short label for the tier-selector row (`"read"` / `"click"` / `"full"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AccessTier::Read => "read",
            AccessTier::Click => "click",
            AccessTier::Full => "full",
        }
    }

    /// One-line description of what the tier permits, shown under the
    /// tier-selector row.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            AccessTier::Read => "screenshots only — no clicks or typing",
            AccessTier::Click => "clicks, scroll, drag — no typing or right-click",
            AccessTier::Full => "clicks, typing, key presses, right-click",
        }
    }
}

/// Which macOS TCC permissions are missing, when `request_access` can't
/// proceed to the app-allowlist panel at all. Parity with
/// `ComputerUseApproval.tsx`'s `ComputerUseTccPanel`.
#[derive(Debug, Clone, Copy)]
pub struct TccState {
    /// Whether Accessibility is granted.
    pub accessibility: bool,
    /// Whether Screen Recording is granted.
    pub screen_recording: bool,
}

/// One requested application (label shown to the user + whether it starts
/// pre-checked — mirrors `ComputerUseAppListPanel`'s `checked: Set<string>`
/// starting fully selected).
#[derive(Debug, Clone)]
pub struct RequestedApp {
    /// Display label (the resolved bundle id when available, else the raw
    /// caller-supplied name).
    pub label: String,
}

/// One in-flight `request_access` round-trip between the tool and the TUI.
#[derive(Debug, Clone)]
pub struct ComputerAccessRequest {
    /// One-sentence explanation shown to the user (`request_access`'s
    /// `reason` field, verbatim).
    pub reason: String,
    /// Apps requested for this grant (already resolved to bundle ids where
    /// possible — see `ComputerTool::resolve_app_identifier`).
    pub apps: Vec<RequestedApp>,
    /// The tier requested for `apps` (defaults to `Full` — see
    /// `tool_computer_use`'s `handle_request_access`).
    pub tier: AccessTier,
    /// Whether `clipboardRead` was requested.
    pub clipboard_read: bool,
    /// Whether `clipboardWrite` was requested.
    pub clipboard_write: bool,
    /// Whether `systemKeyCombos` was requested.
    pub system_key_combos: bool,
    /// `Some` when a required macOS permission (Accessibility / Screen
    /// Recording) is missing — the view shows the TCC panel instead of the
    /// app-allowlist panel until both are granted.
    pub tcc_state: Option<TccState>,
}

/// The user's resolution. An empty `granted_apps` with every flag `false`
/// means "denied" — there is no separate boolean, matching how a dropped
/// `resp_tx` (Esc) is mapped to the same all-empty default by the resolver.
#[derive(Debug, Clone, Default)]
pub struct ComputerAccessResponse {
    /// The subset of the request's `apps` (by label) the user left checked.
    pub granted_apps: Vec<String>,
    /// Whether `clipboardRead` was granted.
    pub clipboard_read: bool,
    /// Whether `clipboardWrite` was granted.
    pub clipboard_write: bool,
    /// Whether `systemKeyCombos` was granted.
    pub system_key_combos: bool,
}

/// One in-flight exchange, constructed by the host resolver and sent over
/// the app channel. The TUI fills `resp_tx` when the user submits, or drops
/// it unsent on cancel (Esc) — the resolver maps a closed channel to the
/// same all-denied [`ComputerAccessResponse::default`].
#[derive(Debug)]
pub struct ComputerAccessExchange {
    /// The access being requested.
    pub request: ComputerAccessRequest,
    /// One-shot reply channel.
    pub resp_tx: oneshot::Sender<ComputerAccessResponse>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ordering_matches_capability_escalation() {
        assert!(AccessTier::Read < AccessTier::Click);
        assert!(AccessTier::Click < AccessTier::Full);
    }

    #[test]
    fn default_response_is_fully_denied() {
        let r = ComputerAccessResponse::default();
        assert!(r.granted_apps.is_empty());
        assert!(!r.clipboard_read);
        assert!(!r.clipboard_write);
        assert!(!r.system_key_combos);
    }
}
