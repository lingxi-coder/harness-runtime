//! `permissions.blockReadsOutsideWorkingDirectories` — the Bash-side producers.
//!
//! The file-tool gate lives in [`crate::policy`] (`sc`); this module holds the
//! shapes the SHELL path uses, where an outside-the-working-directories read is
//! reported as a `safetyCheck` carrying
//! [`SafetyCircuitBreaker::OutsideReadsBlocked`] rather than as `sc`'s
//! `type:"other"` deny.
//!
//! Oracle helpers (2.1.263), byte-locked below:
//!
//! ```js
//! function uL(e){ return e?.type==="safetyCheck" && e.circuitBreaker==="outsideReadsBlocked" }
//!
//! function Op(e){ let o=`${e} names a path that is computed at run time, which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)`;
//!   return {behavior:"ask", message:o, decisionReason:{type:"safetyCheck", reason:o, classifierApprovable:!1, circuitBreaker:"outsideReadsBlocked"}} }
//!
//! function zU(e){ let o=`${e}; under the read block (permissions.blockReadsOutsideWorkingDirectories) a command the shell parser cannot analyze asks the person`;
//!   return {behavior:"ask", message:o, decisionReason:{…same…}, suggestions:[]} }
//! ```
//!
//! Note both builders put the SAME string in `message` and in
//! `decisionReason.reason` — they are not two different texts.

use crate::result::{
    PermissionDecisionReason, PermissionMetadata, PermissionPrompt, PermissionResult,
    SafetyCircuitBreaker,
};

/// Oracle `ov` — re-exported here so shell callers do not reach into
/// [`crate::policy`] for it.
pub use crate::policy::OUTSIDE_READS_BLOCKED_REASON;

/// Oracle `uL(decisionReason)` — is this the read block speaking?
///
/// Callers use it to tell an outside-reads refusal apart from every other
/// `safetyCheck`, because the read block gets its own message at each shell call
/// site instead of the generic one.
#[must_use]
pub fn is_outside_reads_blocked(reason: &PermissionDecisionReason) -> bool {
    matches!(
        reason,
        PermissionDecisionReason::SafetyCheck {
            circuit_breaker: Some(SafetyCircuitBreaker::OutsideReadsBlocked),
            ..
        }
    )
}

/// The shared `{safetyCheck, classifierApprovable:false, circuitBreaker:
/// "outsideReadsBlocked"}` ask, whose prompt message IS the reason.
fn outside_reads_ask(tool_name: &str, text: String) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::SafetyCheck {
            reason: text.clone(),
            classifier_approvable: false,
            circuit_breaker: Some(SafetyCircuitBreaker::OutsideReadsBlocked),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message: text,
            options: Vec::new(),
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Oracle `Op(cmd)` — the command names a path only known at run time.
#[must_use]
pub fn ask_runtime_computed_path(tool_name: &str, command: &str) -> PermissionResult {
    outside_reads_ask(
        tool_name,
        format!(
            "{command} names a path that is computed at run time, which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)"
        ),
    )
}

/// Oracle `zU(reason)` — the shell parser could not analyse the command at all,
/// so the read block escalates to the person. `reason` is the analyser's own
/// explanation, which the oracle prefixes onto its fixed tail.
#[must_use]
pub fn ask_unanalyzable(tool_name: &str, reason: &str) -> PermissionResult {
    outside_reads_ask(
        tool_name,
        format!(
            "{reason}; under the read block (permissions.blockReadsOutsideWorkingDirectories) a command the shell parser cannot analyze asks the person"
        ),
    )
}

/// Oracle `vtt()` — a sed script off the allowlist.
#[must_use]
pub fn ask_sed_off_allowlist(tool_name: &str) -> PermissionResult {
    outside_reads_ask(
        tool_name,
        "This sed script is not on the allowlist and can read or write any file, which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)".to_string(),
    )
}

/// `${cmd} names '${path}', which cannot be checked against the read block
/// (permissions.blockReadsOutsideWorkingDirectories)` — a named path the
/// analyser resolved but cannot classify.
#[must_use]
pub fn ask_uncheckable_named_path(tool_name: &str, command: &str, path: &str) -> PermissionResult {
    outside_reads_ask(
        tool_name,
        format!(
            "{command} names '{path}', which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)"
        ),
    )
}

/// The three call-site texts that name a resolved OUTSIDE path. All end with the
/// same `…, which the read block does not allow without asking
/// (permissions.blockReadsOutsideWorkingDirectories). Add the directory with
/// /add-dir, or remove that setting.` tail; they differ only in how the command
/// reaches the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsidePathShape {
    /// `cd`/`pushd`/`env --chdir`: `${cmd} moves later reads to a directory outside the working directories, …`
    MovesLaterReads,
    /// `ln`/`link`/`cp`/`mv`: `${cmd} names a path outside the working directories, …`
    NamesAPath,
    /// the generic walker: `${cmd} names '${path}', outside the working directories, …`
    NamesResolved,
}

/// The outside-path TEXT for `shape` — shared by the `PermissionResult`
/// builder below and by the shell path walkers, which produce a
/// `PathConstraintAsk` instead.
#[must_use]
pub fn outside_path_message(command: &str, resolved_path: &str, shape: OutsidePathShape) -> String {
    const TAIL: &str = ", which the read block does not allow without asking (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting.";
    let head = match shape {
        OutsidePathShape::MovesLaterReads => {
            format!("{command} moves later reads to a directory outside the working directories")
        }
        OutsidePathShape::NamesAPath => {
            format!("{command} names a path outside the working directories")
        }
        OutsidePathShape::NamesResolved => {
            format!("{command} names '{resolved_path}', outside the working directories")
        }
    };
    format!("{head}{TAIL}")
}

/// Which `OutsidePathShape` the oracle uses for `command`.
///
/// `ln`/`link` (`mpo`) and `cp`/`mv` take the bare `names a path outside …`
/// form; every other verb goes through the generic walker (`gmo`), which
/// interpolates the RESOLVED path.
#[must_use]
pub fn outside_path_shape_for(command: &str) -> OutsidePathShape {
    match command {
        "ln" | "link" | "cp" | "mv" => OutsidePathShape::NamesAPath,
        _ => OutsidePathShape::NamesResolved,
    }
}

/// Build the outside-path ask for `shape`.
#[must_use]
pub fn ask_outside_path(
    tool_name: &str,
    command: &str,
    resolved_path: &str,
    shape: OutsidePathShape,
) -> PermissionResult {
    outside_reads_ask(
        tool_name,
        outside_path_message(command, resolved_path, shape),
    )
}

/// `${cmd} in '${path}' was blocked by a deny rule` — the sibling shape the same
/// shell call sites emit when the refusal came from a RULE rather than the block.
#[must_use]
pub fn deny_rule_message(command: &str, resolved_path: &str) -> String {
    format!("{command} in '{resolved_path}' was blocked by a deny rule")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_of(result: &PermissionResult) -> (String, bool) {
        match result {
            PermissionResult::Ask { reason, prompt, .. } => {
                let PermissionDecisionReason::SafetyCheck {
                    reason,
                    classifier_approvable,
                    circuit_breaker,
                } = reason
                else {
                    panic!("expected a safetyCheck");
                };
                assert_eq!(
                    *circuit_breaker,
                    Some(SafetyCircuitBreaker::OutsideReadsBlocked)
                );
                // The oracle puts the SAME string in both slots.
                assert_eq!(reason, &prompt.message);
                (reason.clone(), *classifier_approvable)
            }
            other => panic!("expected an ask, got {other:?}"),
        }
    }

    #[test]
    fn builders_are_byte_locked() {
        let (msg, approvable) = message_of(&ask_runtime_computed_path("Bash", "cat"));
        assert!(!approvable);
        assert_eq!(msg.as_str(), "cat names a path that is computed at run time, which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)");

        let (msg, _) = message_of(&ask_unanalyzable("Bash", "Parse error"));
        assert_eq!(msg.as_str(), "Parse error; under the read block (permissions.blockReadsOutsideWorkingDirectories) a command the shell parser cannot analyze asks the person");

        let (msg, _) = message_of(&ask_uncheckable_named_path("Bash", "grep", "/x/*"));
        assert_eq!(msg.as_str(), "grep names '/x/*', which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)");

        let (msg, _) = message_of(&ask_sed_off_allowlist("Bash"));
        assert!(msg.starts_with("This sed script is not on the allowlist"));
    }

    #[test]
    fn outside_path_shapes_are_byte_locked() {
        let (msg, _) = message_of(&ask_outside_path(
            "Bash",
            "cd",
            "/etc",
            OutsidePathShape::MovesLaterReads,
        ));
        assert_eq!(msg.as_str(), "cd moves later reads to a directory outside the working directories, which the read block does not allow without asking (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting.");

        let (msg, _) = message_of(&ask_outside_path(
            "Bash",
            "ln",
            "/etc/x",
            OutsidePathShape::NamesAPath,
        ));
        assert_eq!(msg.as_str(), "ln names a path outside the working directories, which the read block does not allow without asking (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting.");

        let (msg, _) = message_of(&ask_outside_path(
            "Bash",
            "grep",
            "/etc/x",
            OutsidePathShape::NamesResolved,
        ));
        assert_eq!(msg.as_str(), "grep names '/etc/x', outside the working directories, which the read block does not allow without asking (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting.");
    }

    #[test]
    fn ul_predicate_matches_only_the_read_block() {
        let blocked = PermissionDecisionReason::SafetyCheck {
            reason: String::new(),
            classifier_approvable: false,
            circuit_breaker: Some(SafetyCircuitBreaker::OutsideReadsBlocked),
        };
        let other = PermissionDecisionReason::SafetyCheck {
            reason: String::new(),
            classifier_approvable: false,
            circuit_breaker: Some(SafetyCircuitBreaker::DangerousRemoval),
        };
        let bare = PermissionDecisionReason::SafetyCheck {
            reason: String::new(),
            classifier_approvable: false,
            circuit_breaker: None,
        };
        assert!(is_outside_reads_blocked(&blocked));
        assert!(!is_outside_reads_blocked(&other));
        assert!(!is_outside_reads_blocked(&bare));
    }

    #[test]
    fn deny_rule_message_is_byte_locked() {
        assert_eq!(
            deny_rule_message("cd", "/etc"),
            "cd in '/etc' was blocked by a deny rule"
        );
    }
}
