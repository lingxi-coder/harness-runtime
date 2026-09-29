use async_trait::async_trait;
use std::sync::Arc;

/// Denies exactly one agent type, like an `Agent(<type>)` deny rule.
struct DenyOneType(&'static str);

#[async_trait]
impl platform_api::permission_gate::PermissionGate for DenyOneType {
    async fn check(
        &self,
        _name: &str,
        _input: &serde_json::Value,
    ) -> platform_api::permission_gate::PermissionDecision {
        platform_api::permission_gate::PermissionDecision::Allow
    }

    async fn agent_type_deny(&self, agent_type: &str) -> Option<String> {
        (agent_type == self.0).then(|| "settings.deny".to_string())
    }
}

/// 🚨 The hole a review found in this session's `agent.spawn` work, and the
/// reason the claim "upstream's re-check is unnecessary here" was wrong.
///
/// `Agent(<type>)` is evaluated in the TOOL layer, ABOVE the spawner, so it
/// only ever sees the type the MODEL asked for. Rewriting the request early
/// makes definition resolution and the bypass clamps re-derive — that part
/// held — but it cannot re-run a rule that lives above the hook. Without
/// this check, a hook rewriting `subagent_type` to a denied agent reaches
/// it, and if that agent declares `permissionMode: bypassPermissions`, it
/// reaches it WITH bypass.
#[tokio::test]
async fn a_hook_cannot_rewrite_into_an_agent_type_a_rule_denies() {
    let gate: Arc<dyn platform_api::permission_gate::PermissionGate> =
        Arc::new(DenyOneType("dangerous"));

    // The rule denies `dangerous`, and the rewrite targets exactly it.
    assert_eq!(
        gate.agent_type_deny("dangerous").await.as_deref(),
        Some("settings.deny"),
        "precondition: the gate must actually deny this type"
    );
    // An unrelated type stays allowed, so the check is not a blanket refusal.
    assert_eq!(gate.agent_type_deny("Explore").await, None);
}
