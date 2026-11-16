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

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Explicit user interaction carried by the trusted computer approval channel.
/// Anchor content is descriptive only; it never authorizes a native effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComputerInteraction {
    /// Ask to temporarily control the whole desktop.
    Takeover {
        /// Optional action-specific reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Explanation displayed before consent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
        /// Preview of the next action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_preview: Option<String>,
        /// Optional visual anchor supplied by the tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<serde_json::Value>,
    },
    /// Ask for applications used by a teaching tour.
    TeachAccess {
        /// Optional action-specific reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Explanation displayed before consent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
        /// Preview of the next action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_preview: Option<String>,
        /// Optional visual anchor supplied by the tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<serde_json::Value>,
    },
    /// Wait for the user before the next teaching step.
    TeachStep {
        /// Optional action-specific reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Explanation of the current step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
        /// Preview of the next step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_preview: Option<String>,
        /// Optional visual anchor supplied by the tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<serde_json::Value>,
    },
}

impl ComputerInteraction {
    /// Whether this request may contain application/capability grants.
    #[must_use]
    pub fn permits_app_grants(&self) -> bool {
        matches!(self, Self::TeachAccess { .. })
    }

    /// Validate a response action against the exact pending interaction kind.
    #[must_use]
    pub fn permits_action(&self, action: &str) -> bool {
        match self {
            Self::Takeover { .. } | Self::TeachAccess { .. } => action == "allow",
            Self::TeachStep { .. } => matches!(action, "continue" | "exit"),
        }
    }
}

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
    /// Optional takeover/teaching confirmation, owned by the trusted host.
    pub interaction: Option<ComputerInteraction>,
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
    /// Explicit interaction choice. `None` means no interaction was approved.
    pub interaction_action: Option<String>,
    /// User-selected interaction preference; does not authorize a takeover.
    /// Only `background` and `full_control` are valid for an app approval.
    pub preferred_mode: Option<String>,
}

impl ComputerAccessRequest {
    /// Reject responses that exceed the stored request, including cross-kind
    /// interaction actions and capability grants smuggled into step approvals.
    #[must_use]
    pub fn allows_response(&self, response: &ComputerAccessResponse) -> bool {
        let has_grants = !response.granted_apps.is_empty()
            || response.clipboard_read
            || response.clipboard_write
            || response.system_key_combos;
        if self.tcc_state.is_some() {
            return !has_grants
                && response.interaction_action.is_none()
                && response.preferred_mode.is_none();
        }
        if response
            .preferred_mode
            .as_deref()
            .is_some_and(|mode| !matches!(mode, "background" | "full_control"))
        {
            return false;
        }
        match (&self.interaction, response.interaction_action.as_deref()) {
            (Some(interaction), Some(action)) if interaction.permits_action(action) => {
                if !interaction.permits_app_grants()
                    && (has_grants || response.preferred_mode.is_some())
                {
                    return false;
                }
            }
            (Some(_), None) if !has_grants && response.preferred_mode.is_none() => return true,
            (None, None) => {}
            _ => return false,
        }
        let mut granted = std::collections::HashSet::new();
        response
            .granted_apps
            .iter()
            .all(|label| self.apps.iter().any(|app| &app.label == label) && granted.insert(label))
            && (!response.clipboard_read || self.clipboard_read)
            && (!response.clipboard_write || self.clipboard_write)
            && (!response.system_key_combos || self.system_key_combos)
    }
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
        assert!(r.interaction_action.is_none());
        assert!(r.preferred_mode.is_none());
    }

    #[test]
    fn interactions_cannot_expand_pending_application_or_capability_grants() {
        let mut request = ComputerAccessRequest {
            reason: "guided step".into(),
            apps: vec![RequestedApp {
                label: "Notes".into(),
            }],
            tier: AccessTier::Full,
            clipboard_read: true,
            clipboard_write: false,
            system_key_combos: false,
            tcc_state: None,
            interaction: Some(ComputerInteraction::Takeover {
                reason: None,
                explanation: None,
                next_preview: None,
                anchor: None,
            }),
        };
        let mut response = ComputerAccessResponse {
            interaction_action: Some("allow".into()),
            ..Default::default()
        };
        assert!(request.allows_response(&response));
        response.clipboard_read = true;
        assert!(!request.allows_response(&response));
        response.clipboard_read = false;
        response.interaction_action = Some("continue".into());
        assert!(!request.allows_response(&response));
        request.interaction = Some(ComputerInteraction::TeachStep {
            reason: None,
            explanation: None,
            next_preview: None,
            anchor: None,
        });
        assert!(request.allows_response(&response));
        response.interaction_action = Some("exit".into());
        assert!(request.allows_response(&response));
        response.granted_apps = vec!["Notes".into()];
        assert!(!request.allows_response(&response));
        request.interaction = Some(ComputerInteraction::TeachAccess {
            reason: None,
            explanation: None,
            next_preview: None,
            anchor: None,
        });
        response.interaction_action = Some("allow".into());
        assert!(request.allows_response(&response));
        response.interaction_action = None;
        assert!(!request.allows_response(&response));
        assert!(request.allows_response(&ComputerAccessResponse::default()));
        response.interaction_action = Some("allow".into());
        response.granted_apps.push("Notes".into());
        assert!(!request.allows_response(&response));
        response.granted_apps.clear();
        request.tcc_state = Some(TccState {
            accessibility: false,
            screen_recording: true,
        });
        assert!(!request.allows_response(&response));
    }

    #[test]
    fn interaction_preference_requires_an_app_approval_and_never_grants_takeover() {
        let mut request = ComputerAccessRequest {
            reason: "Choose interaction mode".into(),
            apps: vec![],
            tier: AccessTier::Full,
            clipboard_read: false,
            clipboard_write: false,
            system_key_combos: false,
            tcc_state: None,
            interaction: None,
        };
        let mut response = ComputerAccessResponse {
            preferred_mode: Some("background".into()),
            ..Default::default()
        };
        assert!(request.allows_response(&response));
        response.preferred_mode = Some("full_control".into());
        assert!(request.allows_response(&response));
        response.preferred_mode = Some("takeover".into());
        assert!(!request.allows_response(&response));
        response.preferred_mode = Some("background".into());
        response.interaction_action = Some("allow".into());
        request.interaction = Some(ComputerInteraction::Takeover {
            reason: None,
            explanation: None,
            next_preview: None,
            anchor: None,
        });
        assert!(!request.allows_response(&response));
        request.interaction = Some(ComputerInteraction::TeachStep {
            reason: None,
            explanation: None,
            next_preview: None,
            anchor: None,
        });
        response.interaction_action = Some("continue".into());
        assert!(!request.allows_response(&response));
        request.interaction = Some(ComputerInteraction::TeachAccess {
            reason: None,
            explanation: None,
            next_preview: None,
            anchor: None,
        });
        response.interaction_action = Some("allow".into());
        assert!(request.allows_response(&response));
        response.interaction_action = None;
        assert!(!request.allows_response(&response));
        request.interaction = None;
        request.tcc_state = Some(TccState {
            accessibility: false,
            screen_recording: true,
        });
        assert!(!request.allows_response(&response));
    }
}
