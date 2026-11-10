//! Invocation-local checked MCP controls cannot persist approval state.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Prompt {
    calls: AtomicUsize,
    auto: bool,
}
#[async_trait::async_trait]
impl PermissionGate for Prompt {
    async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
        panic!("contextual prompt required")
    }
    async fn check_with_context(
        &self,
        _: &str,
        _: &Utf16JsonProjection,
        _: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let updated_input = Some(Utf16JsonProjection::parse(r#"{"changed":true}"#).unwrap());
        if self.auto {
            return PermissionOutcome::AllowAuto { updated_input };
        }
        PermissionOutcome::Allow {
            updated_input,
            permission_updates: vec![Utf16JsonProjection::parse(
                r#"{"type":"setMode","mode":"bypassPermissions","destination":"session"}"#,
            )
            .unwrap()],
            decision_classification: None,
        }
    }
}

#[tokio::test]
async fn checked_mcp_prompts_keep_input_rewrites_but_never_apply_updates_or_auto_mode() {
    for auto in [false, true] {
        let prompt = Arc::new(Prompt {
            calls: AtomicUsize::new(0),
            auto,
        });
        let gate = PolicyPermissionGate::new(
            Arc::new(PermissionPolicy::new(PermissionMode::Default)),
            prompt.clone(),
        );
        let direct = PermissionCheckContext {
            request_source: Some(
                lingxi_core::host::permission_gate::PermissionRequestSource::DirectMcp,
            ),
            suppress_always_allow_rule: true,
            ..Default::default()
        };
        let input = Utf16JsonProjection::parse("{}").unwrap();
        let PermissionOutcome::Allow {
            updated_input,
            permission_updates,
            ..
        } = gate
            .check_prompt_transport("mcp__server__write", &input, &direct)
            .await
        else {
            panic!("direct approval")
        };
        assert_eq!(
            updated_input.unwrap().to_json_string().unwrap(),
            r#"{"changed":true}"#
        );
        assert!(permission_updates.is_empty());
        let ordinary = gate
            .check_prompt_transport(
                "mcp__server__write",
                &input,
                &PermissionCheckContext::default(),
            )
            .await;
        if auto {
            assert!(matches!(ordinary, PermissionOutcome::AllowAuto { .. }));
        } else {
            assert!(
                matches!(ordinary, PermissionOutcome::Allow { permission_updates, .. } if permission_updates.len() == 1)
            );
        }
        assert_eq!(prompt.calls.load(Ordering::SeqCst), 2);
        for mode in [PermissionMode::Plan, PermissionMode::DontAsk] {
            let blocked =
                PolicyPermissionGate::new(Arc::new(PermissionPolicy::new(mode)), prompt.clone());
            assert!(
                matches!(blocked.check_prompt_transport("mcp__server__write", &input, &direct).await, PermissionOutcome::Deny { reason } if reason.string_at("") == Some("nobody can be asked in this permission mode."))
            );
        }
        assert_eq!(
            prompt.calls.load(Ordering::SeqCst),
            2,
            "unavailable modes never invoke the host prompt"
        );
    }
}

#[tokio::test]
async fn host_mcp_denials_replace_only_their_own_ephemeral_bucket() {
    let fixed = PermissionRule {
        value: crate::rule::PermissionRuleValue::from_rule_string("mcp__fixed__write"),
        behavior: PermissionBehavior::Deny,
        source: PermissionRuleSource::McpServerPolicy,
    };
    let gate = PolicyPermissionGate::new(
        Arc::new(PermissionPolicy::from_rules(
            PermissionMode::BypassPermissions,
            vec![fixed],
        )),
        Arc::new(Prompt {
            calls: AtomicUsize::new(0),
            auto: false,
        }),
    );
    let input = Utf16JsonProjection::parse("{}").unwrap();
    let ctx = PermissionCheckContext::default();
    gate.replace_sdk_mcp_denied_tools(vec!["mcp__session__write".into()]);
    for tool in ["mcp__session__write", "mcp__fixed__write"] {
        assert_eq!(
            gate.check_mod_query(tool, &input, &ctx)
                .await
                .unwrap()
                .verdict
                .decision,
            core_permission::ModToolCheckDecision::Deny
        );
        assert!(gate.tool_wide_deny_names().await.contains(&tool.to_owned()));
    }
    gate.replace_sdk_mcp_denied_tools(vec![]);
    assert_ne!(
        gate.check_mod_query("mcp__session__write", &input, &ctx)
            .await
            .unwrap()
            .verdict
            .decision,
        core_permission::ModToolCheckDecision::Deny
    );
    assert_eq!(
        gate.check_mod_query("mcp__fixed__write", &input, &ctx)
            .await
            .unwrap()
            .verdict
            .decision,
        core_permission::ModToolCheckDecision::Deny
    );
}
