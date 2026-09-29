//! The policy-backed answer to oracle `kq` — see
//! [`lingxi_core::host::read_auto_allow`] for what it means and why its unset case
//! is `false`.
//!
//! Both halves are pure functions taking their inputs, so the polarity is
//! testable without a session: getting either backwards fails OPEN at the
//! callers, and a gate that can only be exercised through a live policy is a
//! gate nobody re-checks.

use std::sync::Arc;

use lingxi_core::host::read_auto_allow::ReadAutoAllow;
use serde_json::json;

use crate::policy::PermissionPolicy;
use crate::result::{PermissionDecisionReason, PermissionResult};
use crate::PermissionBehavior;
use crate::PermissionMode;

/// The editing tool `kq` is asked about (`Ft` in the oracle).
const EDIT_TOOL: &str = "Edit";
/// The two tools that count as "the model can read a file" (`rt`, `Ui`).
const READER_TOOLS: [&str; 2] = ["Read", "REPL"];

/// Oracle `mh(e, n)` — the model holds the EDITING tool and has no way to read.
///
/// Note the shape: this is true only when all three hold — the tool list has
/// `Edit`, and has neither `Read` nor `REPL`. It is NOT "Read is missing", and
/// `kq` negates it, so a session with no `Edit` at all is not penalised.
#[must_use]
pub fn edits_without_a_reader(tool_names: &[String]) -> bool {
    let has = |name: &str| tool_names.iter().any(|t| t == name);
    has(EDIT_TOOL) && !READER_TOOLS.iter().any(|r| has(r))
}

/// Oracle `dh(e, n)`'s tail, given the policy's decision for a Read of the path.
///
/// ```js
/// if (r.behavior === "allow") return !0;
/// if (r.behavior !== "ask")   return !1;
/// if (n.mode !== "bypassPermissions") return !1;
/// return !(s?.type === "rule" && s.rule.ruleBehavior === "ask");
/// ```
///
/// The `ask` arm is the subtle one: an `ask` counts as readable ONLY under
/// `bypassPermissions`, and even there NOT when an explicit `ask` RULE produced
/// it — a user who wrote an ask rule for a path has said they want to be asked,
/// and bypass mode does not get to reinterpret that as consent.
#[must_use]
pub fn decision_permits_read(result: &PermissionResult, mode: PermissionMode) -> bool {
    match result {
        PermissionResult::Allow { .. } => true,
        PermissionResult::Deny { .. } => false,
        PermissionResult::Ask { reason, .. } => {
            if mode != PermissionMode::BypassPermissions {
                return false;
            }
            !matches!(
                reason,
                PermissionDecisionReason::MatchedRule { rule, .. }
                    if rule.behavior == PermissionBehavior::Ask
            )
        }
    }
}

/// `kq` over a live policy and the session's tool list.
pub struct PolicyReadAutoAllow {
    policy: Arc<PermissionPolicy>,
    /// `None` means the session's tool list is UNKNOWN, which answers `false`
    /// rather than skipping the `mh` half. An empty list is a different thing —
    /// it is a real answer ("no tools"), and `mh` handles it.
    tool_names: Option<Vec<String>>,
}

impl PolicyReadAutoAllow {
    /// Bind the probe to the session's policy and tool list.
    #[must_use]
    pub fn new(policy: Arc<PermissionPolicy>, tool_names: Vec<String>) -> Self {
        Self {
            policy,
            tool_names: Some(tool_names),
        }
    }

    /// A probe bound to a policy whose tool list is not known here. It answers
    /// `false` for everything — the fail-safe answer — so a host can publish
    /// eagerly without accidentally granting the `mh` half by default.
    #[must_use]
    pub fn without_tool_list(policy: Arc<PermissionPolicy>) -> Self {
        Self {
            policy,
            tool_names: None,
        }
    }
}

impl ReadAutoAllow for PolicyReadAutoAllow {
    fn read_auto_allowed(&self, path: &str) -> bool {
        let Some(tool_names) = self.tool_names.as_deref() else {
            // Unknown tool list ⇒ cannot evaluate `mh` ⇒ not auto-allowed.
            return false;
        };
        if edits_without_a_reader(tool_names) {
            return false;
        }
        // `qE(e, n)` — the non-prompting evaluation of a Read of this path.
        let result = self.policy.authorize("Read", &json!({ "file_path": path }));
        decision_permits_read(&result, self.policy.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::PermissionMetadata;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// `mh` is true ONLY for "has Edit, has no reader". Every other shape is
    /// false, and `kq` negates it — so a session with no Edit at all, or with
    /// either reader present, is never penalised by this half.
    #[test]
    fn edits_without_a_reader_is_narrow() {
        assert!(edits_without_a_reader(&names(&["Edit", "Bash"])));
        assert!(
            !edits_without_a_reader(&names(&["Edit", "Read"])),
            "Read present ⇒ the model could have read it"
        );
        assert!(
            !edits_without_a_reader(&names(&["Edit", "REPL"])),
            "REPL counts as a reader too"
        );
        assert!(
            !edits_without_a_reader(&names(&["Read", "Bash"])),
            "no Edit at all ⇒ this half does not apply"
        );
        assert!(!edits_without_a_reader(&[]));
    }

    fn ask(reason: PermissionDecisionReason) -> PermissionResult {
        PermissionResult::Ask {
            reason,
            prompt: crate::result::PermissionPrompt {
                title: String::new(),
                message: String::new(),
                options: vec![],
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata::default(),
        }
    }

    fn mode_reason() -> PermissionDecisionReason {
        PermissionDecisionReason::PermissionMode {
            mode: PermissionMode::Default,
        }
    }

    /// allow ⇒ readable; deny ⇒ not. The uninteresting arms, pinned so a
    /// refactor cannot quietly invert them.
    #[test]
    fn allow_reads_and_deny_does_not() {
        let allow = PermissionResult::Allow {
            reason: mode_reason(),
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        };
        assert!(decision_permits_read(&allow, PermissionMode::Default));

        let deny = PermissionResult::Deny {
            reason: mode_reason(),
            explanation: None,
            metadata: PermissionMetadata::default(),
        };
        assert!(!decision_permits_read(&deny, PermissionMode::Default));
        assert!(
            !decision_permits_read(&deny, PermissionMode::BypassPermissions),
            "bypass does not turn a DENY into a read"
        );
    }

    /// An `ask` counts as readable only under bypassPermissions.
    #[test]
    fn ask_reads_only_under_bypass() {
        let pending = ask(mode_reason());
        assert!(!decision_permits_read(&pending, PermissionMode::Default));
        assert!(!decision_permits_read(
            &pending,
            PermissionMode::AcceptEdits
        ));
        assert!(decision_permits_read(
            &pending,
            PermissionMode::BypassPermissions
        ));
    }

    /// …but NOT when an explicit `ask` RULE produced it. A user who wrote an ask
    /// rule for a path asked to be consulted; bypass mode does not get to read
    /// that as consent. This is the arm that fails OPEN if it is dropped.
    #[test]
    fn an_explicit_ask_rule_is_not_readable_even_under_bypass() {
        let rule = crate::rule::PermissionRule {
            value: crate::rule::PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("./secret/**".into()),
            },
            behavior: PermissionBehavior::Ask,
            source: crate::rule::PermissionRuleSource::Settings(
                lingxi_core::types::SettingsScope::User,
            ),
        };
        let by_rule = ask(PermissionDecisionReason::MatchedRule { rule: rule.clone() });
        assert!(
            !decision_permits_read(&by_rule, PermissionMode::BypassPermissions),
            "an explicit ask RULE keeps the path unreadable even in bypass mode"
        );

        let mut allow_rule = rule;
        allow_rule.behavior = PermissionBehavior::Allow;
        let by_allow_rule = ask(PermissionDecisionReason::MatchedRule { rule: allow_rule });
        assert!(
            decision_permits_read(&by_allow_rule, PermissionMode::BypassPermissions),
            "only an ASK rule is excluded; another rule that merely led to ask is not"
        );
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// An UNKNOWN tool list is not the same as an empty one: it answers `false`
    /// for every path, so a host that publishes before it knows its tools
    /// cannot accidentally grant the `mh` half.
    #[test]
    fn a_probe_without_a_tool_list_permits_nothing() {
        let policy = Arc::new(PermissionPolicy::new(PermissionMode::BypassPermissions));
        let probe = PolicyReadAutoAllow::without_tool_list(policy);
        assert!(!probe.read_auto_allowed("/tmp/anything"));
        assert!(
            !probe.read_auto_allowed("/"),
            "bypass mode does not rescue an unknown tool list"
        );
    }
}
