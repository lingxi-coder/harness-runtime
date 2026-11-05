//! Native 2.1.318 `Ybn`: explicit Ask still preserves tool-local Deny/Ask.
use std::sync::Arc;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use permission::{
    FsRoots, PermissionBehavior, PermissionDecision, PermissionGate, PermissionMode,
    PermissionPolicy, PermissionResult, PermissionRule, PermissionRuleSource, PermissionRuleValue,
    PolicyPermissionGate,
};

fn policy(mode: PermissionMode) -> PermissionPolicy {
    PermissionPolicy::from_rules(
        mode,
        vec![
            PermissionRule {
                value: PermissionRuleValue::from_rule_string("Bash"),
                behavior: PermissionBehavior::Ask,
                source: PermissionRuleSource::Session,
            },
            PermissionRule {
                value: PermissionRuleValue::from_rule_string("Bash"),
                behavior: PermissionBehavior::Allow,
                source: PermissionRuleSource::Session,
            },
        ],
    )
    .with_roots(FsRoots {
        cwd: "/workspace/project".into(),
        home: Some("/home/test".into()),
        lingxi_home: "/home/test/.lingxi".into(),
    })
}

fn input(command: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&serde_json::json!({"command": command})).unwrap()
}

fn native(name: &str) -> serde_json::Value {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/permission_ask_2_1_318.json")).unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap()["expected"]
        .clone()
}

#[test]
fn wide_ask_cannot_replace_tool_local_clamp_deny() {
    assert_eq!(native("deny")["behavior"], "deny");
    let policy = policy(PermissionMode::BypassPermissions)
        .with_bash_command_clamps(vec![vec!["git status".into()]]);
    assert!(matches!(
        policy.authorize("Bash", &input("curl example.com")),
        PermissionResult::Deny { .. }
    ));
}

#[test]
fn wide_ask_keeps_tool_local_safety_and_path_reason_under_outer_grants() {
    for mode in [
        PermissionMode::Default,
        PermissionMode::Auto,
        PermissionMode::BypassPermissions,
    ] {
        for (command, case) in [("rm -rf /", "safetyAsk"), ("cat /etc/passwd", "pathAsk")] {
            let PermissionResult::Ask {
                reason, metadata, ..
            } = policy(mode).authorize("Bash", &input(command))
            else {
                panic!("{mode:?} {command} must ask")
            };
            assert_eq!(native(case)["matchedAskRule"], true);
            assert!(
                metadata.matched_ask_rule.is_some(),
                "{mode:?} {command}: {reason:?}"
            );
            assert!(metadata.permission_suggestions.is_none());
            assert!(!matches!(
                reason,
                permission::PermissionDecisionReason::MatchedRule { .. }
            ));
        }
    }
}

#[test]
fn a_tool_local_allow_still_asks_for_the_explicit_rule() {
    assert_eq!(native("allow")["matchedAskRule"], false);
    let PermissionResult::Ask {
        reason, metadata, ..
    } = policy(PermissionMode::BypassPermissions).authorize("Bash", &input("echo ok"))
    else {
        panic!("explicit rule must ask")
    };
    assert!(matches!(
        reason,
        permission::PermissionDecisionReason::MatchedRule { .. }
    ));
    assert!(metadata.matched_ask_rule.is_none());
}

struct RejectPrompt;
#[async_trait::async_trait]
impl PermissionGate for RejectPrompt {
    async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: "fixture prompt rejected".into(),
        }
    }
}

#[tokio::test]
async fn matched_ask_cannot_enter_the_auto_accept_edits_fast_path() {
    let gate = PolicyPermissionGate::new(
        Arc::new(policy(PermissionMode::Auto)),
        Arc::new(RejectPrompt),
    );
    assert!(matches!(
        gate.check("Bash", &input("cat /etc/passwd")).await,
        PermissionDecision::Deny { .. }
    ));
    assert!(matches!(
        gate.resolve_detailed("Bash", &input("cat /etc/passwd"))
            .await,
        permission::gate::PermissionResolution::Ask
    ));
}

#[test]
fn plan_passthrough_keeps_a_plain_rule_ask_for_non_file_tools() {
    assert_eq!(native("passthrough")["matchedAskRule"], false);
    let PermissionResult::Ask {
        reason, metadata, ..
    } = policy(PermissionMode::Plan).authorize("Bash", &input("curl example.com"))
    else {
        panic!("must ask")
    };
    assert!(matches!(
        reason,
        permission::PermissionDecisionReason::MatchedRule { .. }
    ));
    assert!(metadata.matched_ask_rule.is_none());
}

#[test]
fn wide_ask_preserves_a_compound_tool_local_reason_tree() {
    let PermissionResult::Ask {
        reason, metadata, ..
    } = policy(PermissionMode::Default)
        .authorize("Bash", &input("cat /etc/passwd && curl example.com"))
    else {
        panic!("must ask")
    };
    assert!(metadata.matched_ask_rule.is_some());
    assert!(
        matches!(
            reason,
            permission::PermissionDecisionReason::SubcommandResults { .. }
        ),
        "{reason:?}"
    );
}

#[cfg(feature = "bash-ast")]
#[test]
fn wide_ask_retains_the_background_operator_check() {
    let mut policy = policy(PermissionMode::BypassPermissions);
    // The exact grant supplies a tool-local Allow before the Bash wrapper's
    // background check. Its resulting Ask must retain the outer ask rule.
    policy
        .allow_rules
        .entry(PermissionRuleSource::Session)
        .or_default()
        .push(PermissionRule {
            value: PermissionRuleValue::from_rule_string("Bash(echo ok &)"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Session,
        });
    let PermissionResult::Ask {
        reason, metadata, ..
    } = policy.authorize("Bash", &input("echo ok &"))
    else {
        panic!("must ask")
    };
    assert!(metadata.matched_ask_rule.is_some());
    assert!(
        matches!(
            reason,
            permission::PermissionDecisionReason::SafetyCheck { .. }
        ),
        "{reason:?}"
    );
}

#[test]
fn remote_bypass_exempts_server_asks_but_never_denies_or_local_asks() {
    for behavior in [PermissionBehavior::Ask, PermissionBehavior::Deny] {
        for remote in [false, true] {
            let mut policy = PermissionPolicy::from_rules(
                PermissionMode::BypassPermissions,
                vec![PermissionRule {
                    value: PermissionRuleValue::from_rule_string("mcp__server__tool"),
                    behavior,
                    source: PermissionRuleSource::McpServerPolicy,
                }],
            );
            policy.remote_mcp_server_policy_bypass_exempt = remote;
            let result = policy.authorize("mcp__server__tool", &input("fixture"));
            assert_eq!(
                matches!(result, PermissionResult::Allow { .. }),
                remote && behavior == PermissionBehavior::Ask
            );
        }
    }
}

struct InspectPrompt;
#[async_trait::async_trait]
impl PermissionGate for InspectPrompt {
    async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        panic!("the prompt must receive its full context")
    }

    async fn check_with_context(
        &self,
        _name: &str,
        _input: &Utf16JsonProjection,
        ctx: &permission::gate::PermissionCheckContext,
    ) -> permission::gate::PermissionOutcome {
        let matched = ctx
            .matched_ask_rule
            .as_ref()
            .expect("native ask substitution must reach the prompt");
        assert_eq!(matched.source, "session");
        assert_eq!(matched.tool_name, "Bash");
        assert_ne!(ctx.decision_reason_type.as_deref(), Some("rule"));
        permission::gate::PermissionOutcome::Deny {
            reason: "fixture rejected".into(),
        }
    }
}

#[tokio::test]
async fn native_ask_provenance_reaches_the_real_prompt_transport() {
    let gate = PolicyPermissionGate::new(
        Arc::new(policy(PermissionMode::Auto)),
        Arc::new(InspectPrompt),
    );
    assert!(matches!(
        gate.check_with_context("Bash", &input("cat /etc/passwd"), &Default::default())
            .await,
        permission::gate::PermissionOutcome::Deny { .. }
    ));
}
