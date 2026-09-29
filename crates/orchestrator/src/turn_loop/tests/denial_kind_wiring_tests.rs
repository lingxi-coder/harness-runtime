use crate::conversation::ConversationOrchestrator;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
};
use crate::turn_loop::dispatch_tool_uses_tracked;
use crate::OrchestratorConfig;
use async_trait::async_trait;
use platform_api::permission_gate::{
    PermissionDecision, PermissionDecisionSource, PermissionGate, PermissionResolution,
};
use protocol::ToolUseId;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// A gate that always denies through the SOURCED resolution, with the
/// structured provenance fields under test control.
struct ProvenanceDenyGate {
    decision_reason_type: Option<String>,
    decision_reason: Option<String>,
    behavior_ask: bool,
}

#[async_trait]
impl PermissionGate for ProvenanceDenyGate {
    async fn check(&self, _t: &str, _i: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: "denied-for-test".into(),
        }
    }
    async fn resolve_detailed(&self, _t: &str, _i: &serde_json::Value) -> PermissionResolution {
        PermissionResolution::Deny {
            reason: "denied-for-test".into(),
            source: PermissionDecisionSource::Rule,
            rule_source: None,
            decision_reason_type: self.decision_reason_type.clone(),
            decision_reason: self.decision_reason.clone(),
            behavior_ask: self.behavior_ask,
            content_blocks: Vec::new(),
        }
    }
}

struct DeniedTool;
#[async_trait]
impl Tool for DeniedTool {
    fn name(&self) -> &str {
        "Denied"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _: &serde_json::Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
        "denied".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        panic!("DeniedTool::call must never run — the gate denies it");
    }
}

/// Dispatch one denied tool through the given gate and return the
/// `(tool_use_id, denial_kind)` pairs the output stream observed.
async fn denial_kinds_for(gate: ProvenanceDenyGate) -> Vec<(ToolUseId, String)> {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(DeniedTool) as Arc<dyn Tool>);
    let output = MockOutputStream::new();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(gate),
        Arc::new(output.clone()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = vec![(ToolUseId::new(), "Denied".to_string(), json!({}), None)];
    dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch must succeed on a denied tool");
    output.denial_snapshot().await
}

/// OR-1 — the denial must also land in the session's `permission_denials`
/// record, which is what the stream-json `result` frame reports.
///
/// The CLI-side test only proves cell → frame; this proves the PRODUCER,
/// i.e. that the deny funnel really records. Without it the feature could be
/// fully plumbed and still report `[]` forever.
#[tokio::test]
async fn a_denied_tool_is_recorded_in_the_sessions_permission_denials() {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(DeniedTool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(ProvenanceDenyGate {
            decision_reason_type: Some("rule".into()),
            decision_reason: None,
            behavior_ask: false,
        }),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    assert!(
        orch.permission_denials().await.is_empty(),
        "precondition: nothing denied yet"
    );

    let id = ToolUseId::new();
    let input = json!({"file_path": "/repo/secret/.env"});
    let uses = vec![(id.clone(), "Denied".to_string(), input.clone(), None)];
    dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch must succeed on a denied tool");

    let denials = orch.permission_denials().await;
    assert_eq!(denials.len(), 1, "the deny funnel must record exactly once");
    assert_eq!(denials[0].tool_name, "Denied");
    assert_eq!(denials[0].tool_use_id, id.to_string());
    assert_eq!(
        denials[0].tool_input, input,
        "tool_input is the input the decision was made on"
    );
}

/// An ALLOWED tool must not be recorded — otherwise `permission_denials`
/// would fill with every call and the field would be worse than empty.
#[tokio::test]
async fn an_allowed_tool_is_not_recorded_as_a_denial() {
    struct AllowGate;
    #[async_trait]
    impl PermissionGate for AllowGate {
        async fn check(&self, _t: &str, _i: &serde_json::Value) -> PermissionDecision {
            PermissionDecision::Allow
        }
    }
    struct OkTool;
    #[async_trait]
    impl Tool for OkTool {
        fn name(&self) -> &str {
            "Ok"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "ok".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                data: json!("fine"),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(OkTool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(AllowGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = vec![(ToolUseId::new(), "Ok".to_string(), json!({}), None)];
    dispatch_tool_uses_tracked(&orch, &uses, None).await.ok();
    assert!(
        orch.permission_denials().await.is_empty(),
        "an allowed call must not be recorded as a denial"
    );
}

/// A plain rule denial reaches the output stream stamped `permission-rule`.
/// This is the value the SDK/stdio transport must carry: claude-code's
/// `JMn` preserves the host's `behavior`, so a host deny is
/// `behavior:"deny"` + a `permissionPromptTool` reason and never takes the
/// `ask` branch. An earlier revision stamped `user-rejected` here.
#[tokio::test]
async fn rule_denial_is_emitted_as_permission_rule() {
    let kinds = denial_kinds_for(ProvenanceDenyGate {
        decision_reason_type: Some("rule".into()),
        decision_reason: None,
        behavior_ask: false,
    })
    .await;
    assert_eq!(
        kinds.len(),
        1,
        "the denied tool must reach emit_tool_result_denied exactly once"
    );
    assert_eq!(kinds[0].1, "permission-rule");
}

/// An auto-mode classifier denial is carried through as `automode-blocked`
/// — proving the classifier reason really is threaded from the sourced
/// resolution arm to the emit site, not just computed locally.
#[tokio::test]
async fn classifier_denial_is_emitted_as_automode_blocked() {
    let kinds = denial_kinds_for(ProvenanceDenyGate {
        decision_reason_type: Some("classifier".into()),
        decision_reason: None,
        behavior_ask: false,
    })
    .await;
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].1, "automode-blocked");
}

/// A tool skipped by the PRE-CANCEL guard is stamped `cancelled`.
///
/// claude-code hardcodes `toolDenialKind:"cancelled"` at this site (binary
/// offset 235399713) — it does NOT route through the `YDd` abort-reason
/// classifier, so no abort-reason plumbing is needed to be faithful here.
#[tokio::test]
async fn pre_cancelled_tool_is_emitted_as_cancelled() {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(DeniedTool) as Arc<dyn Tool>);
    let output = MockOutputStream::new();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(ProvenanceDenyGate {
            decision_reason_type: None,
            decision_reason: None,
            behavior_ask: false,
        }),
        Arc::new(output.clone()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel(); // fire BEFORE dispatch
    let uses = vec![(ToolUseId::new(), "Denied".to_string(), json!({}), None)];
    dispatch_tool_uses_tracked(&orch, &uses, Some(cancel))
        .await
        .expect("dispatch must succeed on a pre-cancelled tool");

    let kinds = output.denial_snapshot().await;
    assert_eq!(
        kinds.len(),
        1,
        "the pre-cancelled tool must report denial provenance"
    );
    assert_eq!(kinds[0].1, "cancelled");
}

/// An `ask`-behavior denial short-circuits to `user-rejected` end-to-end.
#[tokio::test]
async fn ask_behavior_denial_is_emitted_as_user_rejected() {
    let kinds = denial_kinds_for(ProvenanceDenyGate {
        decision_reason_type: Some("classifier".into()),
        decision_reason: None,
        behavior_ask: true,
    })
    .await;
    assert_eq!(kinds.len(), 1);
    assert_eq!(
        kinds[0].1, "user-rejected",
        "behavior_ask must outrank the classifier reason"
    );
}
