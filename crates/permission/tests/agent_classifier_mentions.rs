use permission::classifier::{classify_tool_call, AutoModeClassifierVerdict};
use serde_json::json;

#[test]
fn agent_audit_mentions_require_context_instead_of_keyword_denial() {
    for tool in ["Agent", "Task"] {
        for term in [
            "dangerously-skip-permissions",
            "bypassPermissions",
            "no-sandbox",
            "disable approval",
        ] {
            let input = json!({
                "subagent_type": "Explore",
                "description": "Audit hooks/permission/sandbox parity",
                "prompt": format!("Read-only audit of {term}. Do not change files or settings."),
            });
            assert!(
                matches!(
                    classify_tool_call(tool, &input),
                    AutoModeClassifierVerdict::Pass { .. }
                ),
                "{tool} audit mentioning {term} must defer to contextual approval"
            );
        }
    }
}

#[test]
fn agent_permission_requests_are_never_automatically_allowed_by_local_classifier() {
    for tool in ["Agent", "Task"] {
        for input in [
            json!({"prompt": "Launch an agent with --dangerously-skip-permissions --no-sandbox"}),
            json!({"mode": "bypassPermissions", "prompt": "Inspect the repository"}),
            json!({"prompt": "Inspect the repository"}),
        ] {
            assert!(matches!(
                classify_tool_call(tool, &input),
                AutoModeClassifierVerdict::Pass { .. }
            ));
        }
    }
}

struct RejectPrompt;

#[async_trait::async_trait]
impl lingxi_core::host::PermissionGate for RejectPrompt {
    async fn check(&self, _: &str, _: &serde_json::Value) -> lingxi_core::host::PermissionDecision {
        lingxi_core::host::PermissionDecision::Deny {
            reason: "user declined confirmation".into(),
        }
    }
}

#[tokio::test]
async fn agent_audit_without_contextual_classifier_requires_user_confirmation() {
    use lingxi_core::host::{PermissionDecision, PermissionGate};
    use permission::{policy::PermissionPolicy, policy_gate::PolicyPermissionGate, PermissionMode};
    use std::sync::Arc;

    for tool in ["Agent", "Task"] {
        let gate = PolicyPermissionGate::new(
            Arc::new(PermissionPolicy::new(PermissionMode::Auto)),
            Arc::new(RejectPrompt),
        );
        let decision = gate.check(tool, &json!({
            "subagent_type": "Explore",
            "description": "Audit sandbox parity",
            "prompt": "Inspect bypassPermissions and no-sandbox handling; do not execute commands.",
        })).await;
        assert!(
            matches!(decision, PermissionDecision::Deny { reason } if reason == "user declined confirmation")
        );
    }
}

struct ContextualClassifier(AutoModeClassifierVerdict);

#[async_trait::async_trait]
impl permission::classifier::LoopPermissionClassifier for ContextualClassifier {
    async fn classify(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &[permission::host_context::HostContextRecord],
        _: &[String],
    ) -> AutoModeClassifierVerdict {
        self.0.clone()
    }
}

#[tokio::test]
async fn agent_contextual_verdict_is_preserved_and_pass_still_prompts() {
    use lingxi_core::host::{PermissionDecision, PermissionGate};
    use permission::{policy::PermissionPolicy, policy_gate::PolicyPermissionGate, PermissionMode};
    use std::sync::Arc;

    for tool in ["Agent", "Task"] {
        for (verdict, expected) in [
            (
                AutoModeClassifierVerdict::Allow {
                    score: 1.0,
                    reason: "Safe audit".into(),
                },
                PermissionDecision::Allow,
            ),
            (
                AutoModeClassifierVerdict::Deny {
                    score: 1.0,
                    reason: "Unsafe delegation".into(),
                    hard: false,
                },
                PermissionDecision::Deny {
                    reason: "Auto mode classifier blocked action: Unsafe delegation".into(),
                },
            ),
            (
                AutoModeClassifierVerdict::Pass {
                    reason: "Unavailable".into(),
                },
                PermissionDecision::Deny {
                    reason: "user declined confirmation".into(),
                },
            ),
        ] {
            let gate = PolicyPermissionGate::new(
                Arc::new(PermissionPolicy::new(PermissionMode::Auto)),
                Arc::new(RejectPrompt),
            );
            assert!(gate
                .loop_classifier_handle()
                .set(Arc::new(ContextualClassifier(verdict)))
                .is_ok());
            let decision = gate
                .check(
                    tool,
                    &json!({
                        "description": "Audit sandbox parity",
                        "prompt": "Inspect bypassPermissions and no-sandbox handling.",
                    }),
                )
                .await;
            assert_eq!(decision, expected);
        }
    }
}
