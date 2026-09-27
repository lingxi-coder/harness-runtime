//! `PermissionResult` — outcome of `PermissionPolicy::authorize`.
//!
//! Tagged enum with three variants — `Allow`, `Deny`, `Ask` — each carrying
//! a `PermissionDecisionReason` (why), an optional payload (per-variant),
//! and a shared `PermissionMetadata` (matched rule names for audit).

use crate::mode::PermissionMode;
use crate::rule::PermissionRule;
use protocol::RequestId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::SystemTime;

/// The decision returned by `PermissionPolicy::authorize`.
///
/// Variants are tagged by `behavior` for JSON ergonomics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum PermissionResult {
    /// The tool call is permitted.
    Allow {
        /// Why the call was allowed.
        reason: PermissionDecisionReason,
        /// Optional rewritten input the caller should use instead.
        updated_input: Option<Value>,
        /// Optional destination for persisting a derived rule.
        update_destination: Option<PermissionUpdateDestination>,
        /// Audit metadata (matched rule names, etc.).
        metadata: PermissionMetadata,
    },
    /// The tool call is rejected.
    Deny {
        /// Why the call was denied.
        reason: PermissionDecisionReason,
        /// Optional human-readable explanation surfaced to the user.
        explanation: Option<String>,
        /// Audit metadata.
        metadata: PermissionMetadata,
    },
    /// The decision is deferred to the user.
    Ask {
        /// Why the user is being asked.
        reason: PermissionDecisionReason,
        /// Prompt the host should display.
        prompt: PermissionPrompt,
        /// Optional classifier check still in flight for this request.
        pending_classifier_check: Option<PendingClassifierCheck>,
        /// Audit metadata.
        metadata: PermissionMetadata,
    },
}

/// claude-code `decisionReason.circuitBreaker` — the safety circuit that raised
/// a `safetyCheck`. The complete 2.1.263 value set (grep of the binary for
/// `circuitBreaker:"…"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SafetyCircuitBreaker {
    /// `backgroundOperator` — the `&` background-operator allow→ask downgrade.
    BackgroundOperator,
    /// `dangerousRemoval` — `Dangerous ${cmd} operation …` (oracle `HL`).
    DangerousRemoval,
    /// `isolatePeerMachines`.
    IsolatePeerMachines,
    /// `outsideReadsBlocked` — `permissions.blockReadsOutsideWorkingDirectories`.
    OutsideReadsBlocked,
    /// `restrictedMode`.
    RestrictedMode,
    /// `suspiciousWindowsPath`.
    SuspiciousWindowsPath,
}

/// Discriminated reason for a permission decision.
///
/// Carries enough detail for audit logs and for the UI to explain the choice
/// (matched rule, mode fallback, classifier verdict, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionDecisionReason {
    /// A rule explicitly matched the call.
    MatchedRule {
        /// The matching rule.
        rule: PermissionRule,
    },
    /// The active mode produced the decision (no rule matched).
    PermissionMode {
        /// The mode in force.
        mode: PermissionMode,
    },
    /// A composite call (e.g. piped bash) — per-subcommand reasons.
    SubcommandResults {
        /// Map from subcommand identifier to its individual result.
        reasons: HashMap<String, Box<PermissionResult>>,
    },
    /// A custom external permission-prompt tool returned the decision.
    PermissionPromptTool {
        /// Name of the prompt tool that ran.
        tool_name: String,
    },
    /// A classifier approved the call.
    ClassifierApproved {
        /// Which classifier.
        classifier: ClassifierKind,
        /// Classifier confidence in `[0.0, 1.0]`.
        score: f64,
    },
    /// A classifier rejected the call.
    ClassifierRejected {
        /// Which classifier.
        classifier: ClassifierKind,
        /// Classifier confidence in `[0.0, 1.0]`.
        score: f64,
        /// Human-readable classifier explanation, forwarded as the oracle's
        /// `decisionReason.reason` text.
        reason: String,
    },
    /// A `PreToolUse` hook overrode the rule-based decision.
    HookOverride {
        /// Hook identifier.
        hook_id: String,
        /// Configuration source that registered the hook (optional).
        source: Option<String>,
        /// Free-form reason from the hook (optional).
        reason: Option<String>,
    },
    /// An async sub-agent returned the decision asynchronously.
    AsyncAgent {
        /// Free-form reason from the agent.
        reason: String,
    },
    /// The sandbox layer overrode the decision.
    SandboxOverride {
        /// Why the sandbox intervened.
        reason: SandboxOverrideReason,
    },
    /// A working-directory check produced the decision.
    WorkingDirectory {
        /// Free-form reason (e.g. "path escapes project root").
        reason: String,
    },
    /// A static safety check produced the decision.
    SafetyCheck {
        /// Free-form reason.
        reason: String,
        /// Whether a classifier could still approve this call.
        classifier_approvable: bool,
        /// claude-code `decisionReason.circuitBreaker` — which safety circuit
        /// produced this check. `None` for a check the oracle emits without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        circuit_breaker: Option<SafetyCircuitBreaker>,
    },
    /// Anything not covered above.
    Other {
        /// Free-form reason.
        reason: String,
    },
    /// Consecutive denials exceeded the configured limit.
    DenialLimitExceeded,
    /// `Auto` mode fell back to the parent mode.
    AutoModeFallback,
    /// `BypassPermissions` mode allowed the call.
    BypassPermissions,
}

/// Which classifier produced (or is producing) a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassifierKind {
    /// "Yolo" / auto-accept classifier.
    Yolo,
    /// Bash-command-aware classifier.
    Bash,
    /// Transcript-aware classifier.
    Transcript,
}

/// Why the sandbox overrode the rule-based decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxOverrideReason {
    /// The command is on the sandbox exclude list.
    ExcludedCommand,
    /// The user passed `--dangerously-disable-sandbox`.
    DangerouslyDisableSandbox,
}

/// Audit metadata shared across all `PermissionResult` variants.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionMetadata {
    /// Names of rules that contributed to the decision.
    pub matched_rules: Vec<String>,
    /// Raw `PermissionUpdate[]` suggestions offered with an Ask decision.
    /// Keeping the wire-shaped value here lets the lower permission engine
    /// retain tool-specific suggestions without depending on the CLI control
    /// protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<Value>,
    /// Concrete filesystem path that caused a path-scoped Ask, when the path
    /// validator can report it without parsing a human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
}

/// Prompt payload shown to the user when the decision is `Ask`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPrompt {
    /// Short title for the prompt.
    pub title: String,
    /// Longer message body.
    pub message: String,
    /// Action options offered to the user.
    pub options: Vec<String>,
}

/// A classifier check that is still running when the decision is returned.
///
/// The host watches `request_id` and resolves the prompt when the classifier
/// reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingClassifierCheck {
    /// Which classifier is running.
    pub classifier: ClassifierKind,
    /// The async request identifier.
    pub request_id: RequestId,
    /// When the classifier started.
    pub started_at: SystemTime,
}

/// Where to persist a permission update derived from this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionUpdateDestination {
    /// `~/.lingxi/settings.json`.
    UserSettings,
    /// `.lingxi/settings.json` (project-checked).
    ProjectSettings,
    /// `.lingxi/settings.local.json` (gitignored).
    LocalSettings,
    /// Session-only (lives until the agent exits).
    Session,
    /// Treat the rule as if it came from the CLI.
    CliArg,
}
