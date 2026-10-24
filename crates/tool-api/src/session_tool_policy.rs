//! Immutable, explicitly granted session capabilities and a revocable live lease.

pub use mcp::tool_identity::McpToolIdentity;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Complete authority for an isolated app invocation. Missing tools are denied;
/// there are no wildcard, FQN-prefix, alias, plugin, or Mod grants.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionToolPolicy {
    /// Canonical built-in names selected by the host.
    #[serde(default)]
    pub builtin_tools: HashSet<String>,
    /// Exact per-server, per-revision, per-definition MCP grants.
    #[serde(default)]
    pub mcp_tools: HashSet<McpToolIdentity>,
}

/// All clones share revocation; a captured tool or pending approval cannot keep
/// stale authority alive. A lease cannot be expanded or re-enabled.
#[derive(Clone)]
pub struct SessionToolGrantLease {
    policy: Arc<SessionToolPolicy>,
    revoked: Arc<AtomicBool>,
    owner_cancel: Option<tokio_util::sync::CancellationToken>,
    live_mcp_guard: Option<mcp::tool_identity::McpToolDispatchGuard>,
}

impl std::fmt::Debug for SessionToolGrantLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionToolGrantLease")
            .field("revoked", &self.is_revoked())
            .finish_non_exhaustive()
    }
}
impl SessionToolGrantLease {
    /// Freeze the host-approved capability set for one invocation.
    #[must_use]
    pub fn new(policy: SessionToolPolicy) -> Self {
        Self {
            policy: Arc::new(policy),
            revoked: Arc::new(AtomicBool::new(false)),
            owner_cancel: None,
            live_mcp_guard: None,
        }
    }
    /// Permanently invalidate every copy, including copies held by MCP retries.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    /// Bind a native app invocation's lifetime before publishing any handle.
    /// Stop/version handoff then blocks dispatch synchronously, even while the
    /// host is still draining task futures or a permission UI response.
    #[must_use]
    pub fn with_owner_cancellation(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.owner_cancel = Some(cancel);
        self
    }
    /// Revalidate trusted host configuration at discovery and every RPC retry.
    #[must_use]
    pub fn with_live_mcp_guard(mut self, guard: mcp::tool_identity::McpToolDispatchGuard) -> Self {
        self.live_mcp_guard = Some(guard);
        self
    }
    /// Whether this invocation still has authority.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
            || self
                .owner_cancel
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    }
    /// Check a canonical builtin; model aliases are not accepted as grants.
    #[must_use]
    pub fn allows_builtin(&self, name: &str) -> bool {
        !self.is_revoked() && self.policy.builtin_tools.contains(name)
    }
    /// Check the actual discovered MCP provenance, including definition revision.
    #[must_use]
    pub fn allows_mcp(&self, identity: &McpToolIdentity) -> bool {
        if self.is_revoked() || !self.policy.mcp_tools.contains(identity) {
            return false;
        }
        if self
            .live_mcp_guard
            .as_ref()
            .is_some_and(|guard| !guard(identity))
        {
            self.revoke();
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_lease_is_permanently_revoked() {
        let mut policy = SessionToolPolicy::default();
        policy.builtin_tools.insert("Read".into());
        let lease = SessionToolGrantLease::new(policy);
        let captured = lease.clone();
        assert!(captured.allows_builtin("Read"));
        assert!(!captured.allows_builtin("read"));
        lease.revoke();
        assert!(!captured.allows_builtin("Read"));
    }
    #[test]
    fn replacing_owner_token_never_revives_captured_tool_handles() {
        let mut policy = SessionToolPolicy::default();
        policy.builtin_tools.insert("Read".into());
        let parent = tokio_util::sync::CancellationToken::new();
        let old_owner = parent.child_token();
        let captured =
            SessionToolGrantLease::new(policy.clone()).with_owner_cancellation(old_owner.clone());
        old_owner.cancel();
        let replacement =
            SessionToolGrantLease::new(policy).with_owner_cancellation(parent.child_token());
        assert!(!captured.allows_builtin("Read"));
        assert!(replacement.allows_builtin("Read"));
        parent.cancel();
        assert!(!replacement.allows_builtin("Read"));
        assert!(!captured.allows_builtin("Read"));
    }
    #[test]
    fn changed_live_configuration_permanently_revokes_captured_lease() {
        let identity = McpToolIdentity {
            server_config_id: "host-slot".into(),
            config_revision: "v1".into(),
            raw_tool_name: "lookup".into(),
            definition_fingerprint: "d1".into(),
        };
        let allowed = Arc::new(AtomicBool::new(true));
        let current = allowed.clone();
        let lease = SessionToolGrantLease::new(SessionToolPolicy {
            builtin_tools: HashSet::new(),
            mcp_tools: HashSet::from([identity.clone()]),
        })
        .with_live_mcp_guard(Arc::new(move |_| current.load(Ordering::Acquire)));
        let captured = lease.clone();
        assert!(captured.allows_mcp(&identity));
        allowed.store(false, Ordering::Release);
        assert!(!captured.allows_mcp(&identity));
        allowed.store(true, Ordering::Release);
        assert!(!lease.allows_mcp(&identity));
        assert!(lease.is_revoked());
    }
    #[test]
    fn exact_mcp_identity_requires_every_component() {
        let identity = McpToolIdentity {
            server_config_id: "host-slot".into(),
            config_revision: "config-v1".into(),
            raw_tool_name: "lookup/a".into(),
            definition_fingerprint: "definition-v1".into(),
        };
        let lease = SessionToolGrantLease::new(SessionToolPolicy {
            builtin_tools: HashSet::new(),
            mcp_tools: HashSet::from([identity.clone()]),
        });
        assert!(lease.allows_mcp(&identity));
        for field in 0..4 {
            let mut changed = identity.clone();
            match field {
                0 => changed.server_config_id.push('x'),
                1 => changed.config_revision.push('x'),
                2 => changed.raw_tool_name = "lookup_a".into(),
                _ => changed.definition_fingerprint.push('x'),
            }
            assert!(!lease.allows_mcp(&changed));
        }
        lease.revoke();
        assert!(!lease.allows_mcp(&identity));
    }
}
