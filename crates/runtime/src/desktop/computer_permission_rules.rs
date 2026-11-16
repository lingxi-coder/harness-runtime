//! Generic preauthorization for the Desktop host's dedicated computer-access adapters.
//!
//! The reference adds its builtin Computer Use MCP tools to `allowedTools`.
//! LingXi's host-owned MCP facade uses the same dedicated resolver, device
//! policy and OS checks as its internal combined owner.

#[derive(Clone, Default)]
pub(crate) struct DesktopComputerPermissionIntegration {
    pub transport: bool,
    pub access_resolver: bool,
    pub native_control: bool,
    pub computer_policy: bool,
    pub browser_descriptor: bool,
    pub protocol_tool_names: Vec<String>,
}

pub(crate) fn append_desktop_computer_permission_rules(
    rules: &mut Vec<permission::PermissionRule>,
    integration: DesktopComputerPermissionIntegration,
    allow_managed_permission_rules_only: bool,
) {
    if allow_managed_permission_rules_only || !integration.transport || !integration.access_resolver
    {
        return;
    }
    for (tool_name, enabled) in [
        (
            "computer",
            integration.native_control && integration.computer_policy,
        ),
        ("browser", integration.browser_descriptor),
    ] {
        if enabled {
            rules.push(permission::PermissionRule {
                value: permission::PermissionRuleValue {
                    tool_name: tool_name.into(),
                    rule_content: None,
                    original_rule: None,
                },
                behavior: permission::PermissionBehavior::Allow,
                source: permission::PermissionRuleSource::CliArg,
            });
        }
    }
    if integration.native_control && integration.computer_policy {
        for name in integration.protocol_tool_names {
            rules.push(permission::PermissionRule {
                value: permission::PermissionRuleValue {
                    tool_name: name,
                    rule_content: None,
                    original_rule: None,
                },
                behavior: permission::PermissionBehavior::Allow,
                source: permission::PermissionRuleSource::CliArg,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use permission::gate::{
        ModToolCheckDecision, PermissionCheckContext, PermissionDecision, PermissionGate,
    };
    use permission::{
        PermissionBehavior, PermissionMode, PermissionPolicy, PermissionRule, PermissionRuleSource,
        PermissionRuleValue, PolicyPermissionGate,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn desktop() -> DesktopComputerPermissionIntegration {
        DesktopComputerPermissionIntegration {
            transport: true,
            access_resolver: true,
            native_control: true,
            computer_policy: true,
            browser_descriptor: true,
            protocol_tool_names: vec![
                "mcp__computer-use__request_access".into(),
                "mcp__computer-use__type".into(),
            ],
        }
    }

    #[derive(Default)]
    struct PromptCounter(AtomicUsize);

    #[async_trait]
    impl PermissionGate for PromptCounter {
        async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            PermissionDecision::Deny {
                reason: "unexpected generic prompt".into(),
            }
        }
    }

    fn gate(
        mode: PermissionMode,
        extra: Option<PermissionBehavior>,
    ) -> (PolicyPermissionGate, Arc<PromptCounter>) {
        let mut rules = Vec::new();
        append_desktop_computer_permission_rules(&mut rules, desktop(), false);
        if let Some(behavior) = extra {
            for name in [
                "computer",
                "browser",
                "mcp__computer-use__request_access",
                "mcp__computer-use__type",
            ] {
                rules.push(PermissionRule {
                    value: PermissionRuleValue {
                        tool_name: name.into(),
                        rule_content: None,
                        original_rule: None,
                    },
                    behavior,
                    source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
                });
            }
        }
        let mut policy =
            PermissionPolicy::from_rules_confined(PermissionMode::Default, rules, false);
        policy.set_mode(mode);
        let inner = Arc::new(PromptCounter::default());
        (
            PolicyPermissionGate::new(Arc::new(policy), inner.clone()),
            inner,
        )
    }

    #[test]
    fn only_actual_desktop_host_adapters_are_preauthorized() {
        for integration in [
            DesktopComputerPermissionIntegration {
                transport: false,
                ..desktop()
            },
            DesktopComputerPermissionIntegration {
                access_resolver: false,
                ..desktop()
            },
            DesktopComputerPermissionIntegration::default(),
        ] {
            let mut rules = Vec::new();
            append_desktop_computer_permission_rules(&mut rules, integration, false);
            assert!(rules.is_empty());
        }
        let mut rules = Vec::new();
        append_desktop_computer_permission_rules(&mut rules, desktop(), true);
        assert!(
            rules.is_empty(),
            "managed-only configuration cannot receive host allow rules"
        );
        append_desktop_computer_permission_rules(
            &mut rules,
            DesktopComputerPermissionIntegration {
                native_control: false,
                ..desktop()
            },
            false,
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value.tool_name, "browser");
        rules.clear();
        append_desktop_computer_permission_rules(
            &mut rules,
            DesktopComputerPermissionIntegration {
                computer_policy: false,
                browser_descriptor: false,
                ..desktop()
            },
            false,
        );
        assert!(
            rules.is_empty(),
            "an ordinary transport must not acquire Desktop-specific permissions"
        );
    }

    #[tokio::test]
    async fn default_and_auto_dispatch_reach_the_dedicated_resolver_without_generic_prompt_or_classifier(
    ) {
        let input = Utf16JsonProjection::parse(
            r#"{"action":"request_access","apps":["org.lingxi.test"],"reason":"test"}"#,
        )
        .unwrap();
        for mode in [PermissionMode::Default, PermissionMode::Auto] {
            let (gate, inner) = gate(mode, None);
            for name in [
                "computer",
                "browser",
                "mcp__computer-use__request_access",
                "mcp__computer-use__type",
            ] {
                let evaluation = gate
                    .check_mod_query(name, &input, &PermissionCheckContext::default())
                    .await
                    .unwrap();
                assert_eq!(evaluation.verdict.decision, ModToolCheckDecision::Allow);
                assert_eq!(gate.check(name, &input).await, PermissionDecision::Allow);
            }
            assert_eq!(inner.0.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn explicit_ask_and_deny_still_precede_host_preauthorization() {
        let input = Utf16JsonProjection::parse(r#"{"action":"screenshot"}"#).unwrap();
        for mode in [PermissionMode::Default, PermissionMode::Auto] {
            for (behavior, expected) in [
                (PermissionBehavior::Ask, ModToolCheckDecision::Ask),
                (PermissionBehavior::Deny, ModToolCheckDecision::Deny),
            ] {
                let (gate, inner) = gate(mode, Some(behavior));
                for name in [
                    "computer",
                    "browser",
                    "mcp__computer-use__request_access",
                    "mcp__computer-use__type",
                ] {
                    let evaluation = gate
                        .check_mod_query(name, &input, &PermissionCheckContext::default())
                        .await
                        .unwrap();
                    assert_eq!(evaluation.verdict.decision, expected);
                }
                assert_eq!(
                    inner.0.load(Ordering::SeqCst),
                    0,
                    "inspection must never resolve a policy ask"
                );
            }
        }
    }

    #[tokio::test]
    async fn explicit_combined_owner_restrictions_still_constrain_protocol_members() {
        const NAME: &str = "mcp__computer-use__request_access";
        let input = Utf16JsonProjection::parse(r#"{"apps":["test.app"],"reason":"test"}"#).unwrap();
        for (behavior, expected) in [
            (PermissionBehavior::Ask, ModToolCheckDecision::Ask),
            (PermissionBehavior::Deny, ModToolCheckDecision::Deny),
        ] {
            let mut rules = Vec::new();
            append_desktop_computer_permission_rules(&mut rules, desktop(), false);
            rules.push(PermissionRule {
                value: PermissionRuleValue {
                    tool_name: "computer".into(),
                    rule_content: None,
                    original_rule: None,
                },
                behavior,
                source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            });
            let policy =
                PermissionPolicy::from_rules_confined(PermissionMode::Default, rules, false);
            let gate =
                PolicyPermissionGate::new(Arc::new(policy), Arc::new(PromptCounter::default()));
            let context = PermissionCheckContext {
                mcp_deny_aliases: std::collections::BTreeMap::from([(
                    NAME.into(),
                    vec!["computer".into()],
                )]),
                ..Default::default()
            };
            let evaluation = gate.check_mod_query(NAME, &input, &context).await.unwrap();
            assert_eq!(evaluation.verdict.decision, expected);
        }
    }

    #[tokio::test]
    async fn host_ceiling_is_preserved_for_the_dispatcher() {
        let input = Utf16JsonProjection::parse(r#"{"action":"screenshot"}"#).unwrap();
        let (gate, _) = gate(PermissionMode::Default, None);
        for ceiling in [
            lingxi_core::host::McpPermissionCeiling::Ask,
            lingxi_core::host::McpPermissionCeiling::Deny,
        ] {
            for name in [
                "computer",
                "browser",
                "mcp__computer-use__request_access",
                "mcp__computer-use__type",
            ] {
                let context = PermissionCheckContext {
                    tool_check_ceiling: Some(ceiling),
                    ..Default::default()
                };
                let evaluation = gate.check_mod_query(name, &input, &context).await.unwrap();
                // The dispatcher composes this ceiling with the generic result
                // and the tool's own check; generic preauthorization cannot drop it.
                assert_eq!(evaluation.ceiling, Some(ceiling));
            }
        }
    }
}
