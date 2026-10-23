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
#[derive(Debug, Clone)]
pub struct SessionToolGrantLease {
    policy: Arc<SessionToolPolicy>,
    revoked: Arc<AtomicBool>,
}

impl SessionToolGrantLease {
    /// Freeze the host-approved capability set for one invocation.
    #[must_use]
    pub fn new(policy: SessionToolPolicy) -> Self {
        Self {
            policy: Arc::new(policy),
            revoked: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Permanently invalidate every copy, including copies held by MCP retries.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    /// Whether this invocation still has authority.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
    /// Check a canonical builtin; model aliases are not accepted as grants.
    #[must_use]
    pub fn allows_builtin(&self, name: &str) -> bool {
        !self.is_revoked() && self.policy.builtin_tools.contains(name)
    }
    /// Check the actual discovered MCP provenance, including definition revision.
    #[must_use]
    pub fn allows_mcp(&self, identity: &McpToolIdentity) -> bool {
        !self.is_revoked() && self.policy.mcp_tools.contains(identity)
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
