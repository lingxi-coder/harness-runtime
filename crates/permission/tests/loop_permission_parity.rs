use permission::policy::PermissionPolicy;
use permission::rule::{
    PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue,
};
use permission::{PermissionMode, PermissionResult};
use serde_json::json;

fn rule(name: &str, behavior: PermissionBehavior) -> PermissionRule {
    PermissionRule {
        value: PermissionRuleValue {
            tool_name: name.into(),
            rule_content: None,
        },
        behavior,
        source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
    }
}

#[test]
fn loop_tools_match_270_tool_local_baselines_without_rules() {
    for mode in [
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::DontAsk,
        PermissionMode::BypassPermissions,
        PermissionMode::Auto,
    ] {
        let policy = PermissionPolicy::new(mode);
        for name in ["CronCreate", "CronDelete", "CronList", "ScheduleWakeup"] {
            let result = policy.authorize(
                name,
                &json!({"cron":"* * * * *", "prompt":"check", "id":"job"}),
            );
            if mode == PermissionMode::Auto && matches!(name, "CronCreate" | "ScheduleWakeup") {
                assert!(
                    matches!(result, PermissionResult::Ask { .. }),
                    "{name} must reach auto classifier"
                );
            } else {
                assert!(
                    matches!(result, PermissionResult::Allow { .. }),
                    "{name} {mode:?}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn loop_tool_baselines_do_not_override_explicit_rules() {
    for name in ["CronCreate", "CronDelete", "CronList", "ScheduleWakeup"] {
        for mode in [
            PermissionMode::Default,
            PermissionMode::Auto,
            PermissionMode::BypassPermissions,
        ] {
            for behavior in [
                PermissionBehavior::Deny,
                PermissionBehavior::Ask,
                PermissionBehavior::Allow,
            ] {
                let policy = PermissionPolicy::from_rules(mode, vec![rule(name, behavior)]);
                let result = policy.authorize(name, &json!({}));
                assert!(
                    match behavior {
                        PermissionBehavior::Deny => matches!(result, PermissionResult::Deny { .. }),
                        PermissionBehavior::Ask => matches!(result, PermissionResult::Ask { .. }),
                        PermissionBehavior::Allow =>
                            matches!(result, PermissionResult::Allow { .. }),
                    },
                    "{name} {mode:?} {behavior:?}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn monitor_retains_outer_name_rules_before_nested_bash_check() {
    for behavior in [PermissionBehavior::Deny, PermissionBehavior::Ask] {
        let policy =
            PermissionPolicy::from_rules(PermissionMode::Default, vec![rule("Monitor", behavior)]);
        let result = policy.authorize("Monitor", &json!({"command":"echo ready"}));
        assert!(
            match behavior {
                PermissionBehavior::Deny => matches!(result, PermissionResult::Deny { .. }),
                _ => matches!(result, PermissionResult::Ask { .. }),
            },
            "{behavior:?}: {result:?}"
        );
    }
}

#[test]
fn unrelated_mutating_tools_still_require_permission() {
    let policy = PermissionPolicy::new(PermissionMode::Default);
    assert!(matches!(
        policy.authorize("TaskStop", &json!({"task_id":"task"})),
        PermissionResult::Ask { .. }
    ));
}

#[test]
fn monitor_outer_allow_applies_after_nested_bash_checks() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        vec![rule("Monitor", PermissionBehavior::Allow)],
    );
    assert!(matches!(
        policy.authorize("Monitor", &json!({"command":"custom-watch --events"})),
        PermissionResult::Allow { .. }
    ));
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        vec![
            rule("Monitor", PermissionBehavior::Allow),
            rule("Bash", PermissionBehavior::Deny),
        ],
    );
    assert!(matches!(
        policy.authorize("Monitor", &json!({"command":"custom-watch --events"})),
        PermissionResult::Deny { .. }
    ));
}

#[test]
fn monitor_outer_allow_preserves_nested_shell_safety_objection() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        vec![rule("Monitor", PermissionBehavior::Allow)],
    )
    .with_roots(permission::filesystem::FsRoots {
        cwd: "/workspace/project".into(),
        home: Some("/home/developer".into()),
        lingxi_home: "/home/developer/.lingxi".into(),
    });
    assert!(matches!(
        policy.authorize("Monitor", &json!({"command":"rm -rf /"})),
        PermissionResult::Ask { .. } | PermissionResult::Deny { .. }
    ));
}

#[tokio::test]
async fn loop_permissions_reach_outer_transport_only_when_required() {
    use lingxi_core::host::{PermissionDecision, PermissionGate};
    use permission::policy_gate::PolicyPermissionGate;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Prompt(AtomicUsize);
    #[async_trait::async_trait]
    impl PermissionGate for Prompt {
        async fn check(&self, _: &str, _: &serde_json::Value) -> PermissionDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            PermissionDecision::Deny {
                reason: "transport reached".into(),
            }
        }
    }
    for mode in [PermissionMode::Default, PermissionMode::Auto] {
        for name in ["CronCreate", "CronDelete", "CronList", "ScheduleWakeup"] {
            let prompt = Arc::new(Prompt(AtomicUsize::new(0)));
            let gate =
                PolicyPermissionGate::new(Arc::new(PermissionPolicy::new(mode)), prompt.clone());
            let decision = gate.check(name, &json!({"prompt":"check"})).await;
            let requires_review =
                mode == PermissionMode::Auto && matches!(name, "CronCreate" | "ScheduleWakeup");
            assert_eq!(
                prompt.0.load(Ordering::SeqCst),
                usize::from(requires_review),
                "{name} {mode:?}"
            );
            assert_eq!(
                matches!(decision, PermissionDecision::Allow),
                !requires_review
            );
        }
    }
}

#[tokio::test]
async fn auto_loop_permission_uses_bound_llm_classifier_and_preserves_rules() {
    use lingxi_core::host::{PermissionDecision, PermissionGate};
    use permission::classifier::{AutoModeClassifierVerdict, LoopPermissionClassifier};
    use permission::policy_gate::PolicyPermissionGate;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Classifier(AtomicUsize);
    #[async_trait::async_trait]
    impl LoopPermissionClassifier for Classifier {
        async fn classify(
            &self,
            _: &str,
            _: &serde_json::Value,
            _: &[permission::host_context::HostContextRecord],
            _: &[String],
        ) -> AutoModeClassifierVerdict {
            self.0.fetch_add(1, Ordering::SeqCst);
            AutoModeClassifierVerdict::Allow {
                score: 1.0,
                reason: "Allowed by fast classifier".into(),
            }
        }
    }
    struct NoPrompt;
    #[async_trait::async_trait]
    impl PermissionGate for NoPrompt {
        async fn check(&self, _: &str, _: &serde_json::Value) -> PermissionDecision {
            panic!("the LLM verdict must bypass prompt transport")
        }
    }
    let classifier = Arc::new(Classifier(AtomicUsize::new(0)));
    for name in ["CronCreate", "ScheduleWakeup", "Monitor"] {
        let gate = PolicyPermissionGate::new(
            Arc::new(PermissionPolicy::new(PermissionMode::Auto)),
            Arc::new(NoPrompt),
        );
        assert!(gate
            .loop_classifier_handle()
            .set(classifier.clone())
            .is_ok());
        assert!(matches!(
            gate.check(name, &json!({"command":"custom-watch"})).await,
            PermissionDecision::Allow
        ));
        let gate = PolicyPermissionGate::new(
            Arc::new(PermissionPolicy::from_rules(
                PermissionMode::Auto,
                vec![rule(name, PermissionBehavior::Deny)],
            )),
            Arc::new(NoPrompt),
        );
        assert!(gate
            .loop_classifier_handle()
            .set(classifier.clone())
            .is_ok());
        assert!(matches!(
            gate.check(name, &json!({"command":"custom-watch"})).await,
            PermissionDecision::Deny { .. }
        ));
    }
    assert_eq!(classifier.0.load(Ordering::SeqCst), 3);
}
