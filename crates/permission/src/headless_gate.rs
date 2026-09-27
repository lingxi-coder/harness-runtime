//! `DenyOnAskGate` — the non-interactive inner permission transport.
//!
//! [`crate::PolicyPermissionGate`] consults its INNER gate only when a tool's
//! policy outcome is `Ask` AND the tool is not auto-allowed (a mutating /
//! [`crate::gate::PromptDefault::DenyByDefault`] tool with no matching allow
//! rule). In a HEADLESS / `--print` session there is no interactive prompt to
//! surface that `Ask`, so — matching claude-code's non-interactive behavior,
//! where a tool call with no `canUseTool` resolution is rejected rather than
//! silently allowed — this gate DENIES.
//!
//! Allow/deny RULES, the active permission MODE, and read-only auto-allow are
//! all resolved by [`crate::PolicyPermissionGate`] BEFORE it delegates here, so
//! this gate only ever turns an otherwise-unresolvable `Ask` into a denial —
//! never overriding an explicit allow rule or a read-only tool.
//!
//! ## Byte-exact deny message (`GRu`)
//!
//! claude-code 2.1.211 denies an unpromptable ask
//! (`shouldAvoidPermissionPrompts`) with
//! `decisionReason:{type:"asyncAgent",reason:"Permission prompts are not
//! available in this context"}` and `message:GRu(e.name)`, where
//! `` GRu(e) = `Permission to use ${e} has been denied. ${Rws}` `` and `Rws`
//! is the shared workaround-guidance block
//! ([`crate::policy_gate::DENIAL_WORKAROUND_GUIDANCE`]). This gate reproduces
//! that message byte-for-byte.

use crate::gate::{PermissionDecision, PermissionGate};
use crate::policy_gate::DENIAL_WORKAROUND_GUIDANCE;
use async_trait::async_trait;
use serde_json::Value;

/// Non-interactive inner gate: turns an unresolved `Ask` into a `Deny`
/// (claude-code headless / `--print` parity). See the module docs for why this
/// is safe to use only as the INNER gate under [`crate::PolicyPermissionGate`].
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyOnAskGate;

/// The byte-exact claude-code `GRu(tool)` headless deny message:
/// `` `Permission to use ${tool} has been denied. ${Rws}` ``.
pub fn headless_deny_message(name: &str) -> String {
    format!("Permission to use {name} has been denied. {DENIAL_WORKAROUND_GUIDANCE}")
}

#[async_trait]
impl PermissionGate for DenyOnAskGate {
    async fn check(&self, name: &str, _input: &Value) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: headless_deny_message(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn denies_with_byte_exact_gru_message() {
        let gate = DenyOnAskGate;
        match gate.check("Bash", &serde_json::json!({})).await {
            PermissionDecision::Deny { reason } => {
                // GRu("Bash") = "Permission to use Bash has been denied. " + Rws
                assert!(
                    reason.starts_with("Permission to use Bash has been denied. "),
                    "reason must be the byte-exact GRu message: {reason}"
                );
                assert!(
                    reason.contains(
                        "IMPORTANT: You *may* attempt to accomplish this action using other tools"
                    ),
                    "reason must carry the workaround guidance block: {reason}"
                );
                assert!(
                    reason.ends_with("Let the user decide how to proceed."),
                    "reason must end with the Rws suffix: {reason}"
                );
                // No trace of the fabricated legacy wording.
                assert!(
                    !reason.contains("non-interactive session"),
                    "the invented message must be gone: {reason}"
                );
            }
            PermissionDecision::Allow => panic!("DenyOnAskGate must never allow"),
        }
    }
}
