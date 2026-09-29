//! `PromptingGate` sub-trait — interactive y/N permission UX.
//!
//! Adds a `prompt_user` method on top of [`PermissionGate`]. Production
//! impls (the `InteractivePromptingGate` in `lingxi-permission`) drive the
//! prompt against stdin/stderr; tests pipe scripted I/O via
//! `tokio::io::duplex`. See M5-05 plan §"Design locks" for the wire-locked
//! prompt formats.
#![forbid(unsafe_code)]

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::host::permission_gate::PermissionGate;

/// Per-tool default decision when the user just presses Enter on the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDefault {
    /// `[Y/n]` — bare Enter ⇒ Allow.
    AllowByDefault,
    /// `[y/N]` — bare Enter ⇒ Deny.
    DenyByDefault,
}

/// A single permission prompt — three variants (M6-05).
///
/// - `ToolUseConfirm` is the M5-05 stdio-prompt case (preserved, bit-identical
///   to the M5-05 struct fields).
/// - `ExitPlanMode` asks the user to approve a plan-mode exit.
/// - `BypassPermissionsMode` asks the user to opt in to dangerous mode.
#[derive(Debug, Clone)]
pub enum PermissionRequest {
    /// Generic per-tool permission confirmation — the M5-05 case, preserved.
    ToolUseConfirm {
        /// Canonical tool name (e.g. `"Bash"`, `"Agent"`).
        tool_name: String,
        /// The model's `tool_input` JSON (preserved for context; not currently
        /// shown in the M5-05 prompt — M5-06 hooks may use it).
        tool_input: Value,
        /// Default decision when the user presses Enter only.
        default_decision: PromptDefault,
        /// §27b — oracle `suppressesAlwaysAllowRule` (client.ts factory,
        /// @182520462). `true` when the tool needs fresh interaction on every
        /// call (`_meta.anthropic/requiresUserInteraction`), so a stored
        /// "always allow" grant would be recorded but then ignored by the
        /// tool. The dialog must omit the "Yes, allow always" option in that
        /// case (`tui/src/bottom_pane/permission_view.rs`), and the gate that
        /// builds this request must never persist a rule from a resolution
        /// against a suppressed request (`tui/src/permission_gate.rs`).
        suppress_always_allow_rule: bool,
    },
    /// Plan-mode exit — user must approve a proposed plan markdown body.
    ExitPlanMode {
        /// Plan markdown body, rendered as a multi-line block in the dialog.
        plan: String,
    },
    /// Dangerous-mode toggle — user must explicitly type `yes` to enable.
    BypassPermissionsMode,
}

/// Outcome of a TUI permission-dialog round-trip (M6-05).
///
/// Maps to [`crate::host::permission_gate::PermissionDecision`] in the TUI gate:
/// `AllowOnce`, `AllowAlways`, and `AllowAuto` all → `Allow`; `Deny` →
/// `Deny { reason }`. `AllowAuto` is handled as a separate rich outcome so the
/// orchestrator can atomically request the live mode transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponse {
    /// Allow this single tool call. Does not persist a session rule.
    AllowOnce,
    /// Allow this tool for the rest of the session (a session rule is
    /// appended to the orchestrator's in-memory rule list).
    AllowAlways,
    /// Allow this call and request an atomic transition to Auto mode. This is
    /// intentionally not persistent and is only emitted for engine-eligible
    /// prompt rows.
    AllowAuto,
    /// Reject the tool call.
    Deny,
}

impl PermissionResponse {
    /// Whether the response should be persisted as a session rule.
    #[must_use]
    pub fn persist(self) -> bool {
        matches!(self, Self::AllowAlways)
    }
}

/// Outcome of one prompt round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDecision {
    /// True ⇒ Allow, false ⇒ Deny.
    pub allow: bool,
    /// Free-form audit reason.
    pub reason: String,
    /// True ⇒ persist this decision (future: write to settings). M5-05 always
    /// sets this to `false` (session-only) — M5-11 `/permissions` adds the
    /// persistence path.
    pub persist: bool,
}

/// Errors specific to the prompting path.
#[derive(Debug, Error)]
pub enum PromptError {
    /// stdin / stderr I/O failure.
    #[error("prompt io: {0}")]
    Io(String),
    /// User typed 3 invalid inputs in a row.
    #[error("prompt invalid input after {attempts} attempts")]
    InvalidInput {
        /// How many invalid inputs were consumed (always 3 at the cap).
        attempts: u32,
    },
    /// stdin closed / user cancelled before answering.
    #[error("prompt cancelled: {reason}")]
    Cancelled {
        /// Free-form reason.
        reason: String,
    },
}

/// Interactive permission gate — extends [`PermissionGate`] with a
/// stdin/stderr prompt round-trip.
#[async_trait]
pub trait PromptingGate: PermissionGate {
    /// Drive one prompt round-trip. Returns `Ok(PromptDecision)` on a valid
    /// answer, `Err(PromptError::InvalidInput)` after 3 invalid inputs.
    async fn prompt_user(&self, request: &PermissionRequest)
        -> Result<PromptDecision, PromptError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_request_constructs_with_allow_default() {
        let req = PermissionRequest::ToolUseConfirm {
            tool_name: "Read".to_string(),
            tool_input: json!({ "path": "/tmp/foo" }),
            default_decision: PromptDefault::AllowByDefault,
            suppress_always_allow_rule: false,
        };
        match req {
            PermissionRequest::ToolUseConfirm {
                tool_name,
                default_decision,
                ..
            } => {
                assert_eq!(tool_name, "Read");
                assert_eq!(default_decision, PromptDefault::AllowByDefault);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn permission_request_constructs_with_deny_default() {
        let req = PermissionRequest::ToolUseConfirm {
            tool_name: "Bash".to_string(),
            tool_input: json!({ "command": "rm -rf /" }),
            default_decision: PromptDefault::DenyByDefault,
            suppress_always_allow_rule: false,
        };
        match req {
            PermissionRequest::ToolUseConfirm {
                tool_name,
                default_decision,
                ..
            } => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(default_decision, PromptDefault::DenyByDefault);
            }
            _ => panic!("wrong variant"),
        }
    }

    // M6-05 Task 1: new enum variants.
    #[test]
    fn permission_request_enum_exit_plan_mode_variant() {
        let req = PermissionRequest::ExitPlanMode {
            plan: "1. Foo\n2. Bar".to_string(),
        };
        match req {
            PermissionRequest::ExitPlanMode { plan } => assert!(plan.contains("Foo")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn permission_request_enum_bypass_permissions_variant() {
        let req = PermissionRequest::BypassPermissionsMode;
        assert!(matches!(req, PermissionRequest::BypassPermissionsMode));
    }

    #[test]
    fn permission_response_three_variants() {
        assert_eq!(PermissionResponse::AllowOnce, PermissionResponse::AllowOnce);
        assert_ne!(
            PermissionResponse::AllowOnce,
            PermissionResponse::AllowAlways
        );
        assert_ne!(PermissionResponse::AllowOnce, PermissionResponse::Deny);
        assert_ne!(PermissionResponse::AllowOnce, PermissionResponse::AllowAuto);
    }

    #[test]
    fn permission_response_persist_only_for_allow_always() {
        assert!(!PermissionResponse::AllowOnce.persist());
        assert!(PermissionResponse::AllowAlways.persist());
        assert!(!PermissionResponse::Deny.persist());
        assert!(!PermissionResponse::AllowAuto.persist());
    }

    #[test]
    fn prompt_decision_constructs() {
        let d = PromptDecision {
            allow: true,
            reason: "user typed y".to_string(),
            persist: false,
        };
        assert!(d.allow);
        assert_eq!(d.reason, "user typed y");
        assert!(!d.persist);
    }

    #[test]
    fn prompt_default_is_copy() {
        let a = PromptDefault::AllowByDefault;
        let b = a; // copy
        assert_eq!(a, b);
    }
}
