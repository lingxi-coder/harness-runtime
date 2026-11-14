//! MCP connector alias rules checked against the 2.1.322 permission contract.
use lingxi_core::host::permission_gate::{ModToolCheckDecision, PermissionCheckContext};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use permission::{
    PermissionBehavior as B, PermissionDecision, PermissionGate, PermissionMode, PermissionPolicy,
    PermissionResult, PermissionRule, PermissionRuleSource as S, PermissionRuleValue,
    PolicyPermissionGate,
};
use std::{collections::BTreeMap, sync::Arc};

const TOOL: &str = "mcp__host-key__ping";
const ALIAS: &str = "mcp__crm_proxy__ping";

fn rules(behavior: B, spelling: &str, rule_source: S) -> Vec<PermissionRule> {
    vec![PermissionRule {
        value: PermissionRuleValue::from_rule_string(spelling),
        behavior,
        source: rule_source,
    }]
}
fn aliases(name: &str) -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([(TOOL.to_owned(), vec![name.to_owned()])])
}

#[test]
fn connector_names_restrict_deny_and_ask_even_without_proxy_expansion() {
    for spelling in [ALIAS, "mcp__crm_proxy", "mcp__crm_proxy__*"] {
        for rule_source in [S::Session, S::CliArg, S::ToolsNarrowing] {
            let policy = PermissionPolicy::from_rules(
                PermissionMode::Default,
                rules(B::Deny, spelling, rule_source),
            )
            .with_mcp_deny_aliases(aliases(ALIAS));
            assert!(
                matches!(
                    policy.authorize(TOOL, &Source::parse("{}").unwrap()),
                    PermissionResult::Deny { .. }
                ),
                "{spelling} {rule_source:?}"
            );
            let policy = PermissionPolicy::from_rules(
                PermissionMode::Default,
                rules(B::Ask, spelling, rule_source),
            )
            .with_mcp_deny_aliases(aliases(ALIAS));
            let PermissionResult::Ask { reason, .. } =
                policy.authorize(TOOL, &Source::parse("{}").unwrap())
            else {
                panic!("must ask")
            };
            assert!(
                matches!(
                    reason,
                    permission::PermissionDecisionReason::MatchedRule { .. }
                ),
                "must retain the alias rule, not an unmatched mode ask"
            );
        }
    }
}

#[test]
fn connector_names_never_satisfy_allow_rules() {
    for spelling in [ALIAS, "mcp__crm_proxy", "mcp__crm_proxy__*"] {
        let policy = PermissionPolicy::from_rules(
            PermissionMode::Default,
            rules(B::Allow, spelling, S::Session),
        )
        .with_mcp_deny_aliases(aliases(ALIAS));
        assert!(
            !matches!(
                policy.authorize(TOOL, &Source::parse("{}").unwrap()),
                PermissionResult::Allow { .. }
            ),
            "{spelling}"
        );
    }
}

struct UnusedPrompt;
#[async_trait::async_trait]
impl PermissionGate for UnusedPrompt {
    async fn check(&self, _: &str, _: &Source) -> PermissionDecision {
        panic!("tool-check snapshots must not prompt")
    }
}

#[tokio::test]
async fn concurrent_private_connector_aliases_never_replace_another_calls_context() {
    let gate = PolicyPermissionGate::new(
        Arc::new(PermissionPolicy::from_rules(
            PermissionMode::Default,
            rules(B::Deny, ALIAS, S::Session),
        )),
        Arc::new(UnusedPrompt),
    );
    let restricted = PermissionCheckContext {
        mcp_deny_aliases: aliases(ALIAS),
        ..Default::default()
    };
    let unrelated = PermissionCheckContext {
        mcp_deny_aliases: aliases("mcp__erp__ping"),
        ..Default::default()
    };
    let input = Source::parse("{}").unwrap();
    let (denied, other) = tokio::join!(
        gate.check_mod_query(TOOL, &input, &restricted),
        gate.check_mod_query(TOOL, &input, &unrelated)
    );
    assert_eq!(denied.unwrap().verdict.decision, ModToolCheckDecision::Deny);
    assert_ne!(other.unwrap().verdict.decision, ModToolCheckDecision::Deny);
    let later = gate
        .check_mod_query(TOOL, &input, &PermissionCheckContext::default())
        .await
        .unwrap();
    assert_ne!(later.verdict.decision, ModToolCheckDecision::Deny);
}
