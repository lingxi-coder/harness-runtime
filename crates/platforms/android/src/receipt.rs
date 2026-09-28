//! Enforcement receipt (spec r3 §`AndroidMinijailProcessRunner`): what was
//! ACTUALLY applied to a jailed run. Attached to the process log / tool meta.
//!
//! The receipt is the honest record of the jail: it never overstates a
//! guarantee. `landlock_enforced` is hard-`false` in v1 (no FS confinement
//! path exists) and `fs_confinement` records the real shape (`app_uid_only`).

use crate::policy::{AndroidSandboxPlan, ExecTarget, NetProfile};
use mobile_linux_api::SandboxBackend;

/// Honest record of one jailed execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidSandboxReceipt {
    /// Always `AndroidMinijail`.
    pub backend: SandboxBackend,
    /// `"SystemShell"` or the helper name.
    pub target: String,
    /// Helper hash when a bundled helper (P4+); `None` for system sh.
    pub target_hash: Option<String>,
    /// `no_new_privs` was set.
    pub no_new_privs: bool,
    /// Net-deny seccomp filter applied.
    pub net_deny: bool,
    /// The seccomp policy name + hash, when a filter was applied (`name@hash`).
    pub seccomp_policy: Option<String>,
    /// Rlimit resources applied (string forms for the log).
    pub rlimits: Vec<String>,
    /// Landlock was NOT applied (always false in v1 — recorded for honesty).
    pub landlock_enforced: bool,
    /// fs confinement reality.
    pub fs_confinement: String,
    /// Features the policy requested but the platform cannot enforce.
    pub unsupported_required_features: Vec<String>,
}

impl AndroidSandboxReceipt {
    /// Build the planned receipt from the prepared plan (pre-execution preview;
    /// the runner stamps execution outcome separately in logs).
    #[must_use]
    pub fn from_plan(plan: &AndroidSandboxPlan) -> Self {
        let (target, target_hash) = match &plan.target {
            ExecTarget::SystemShell => ("SystemShell".to_string(), None),
            ExecTarget::BundledHelper { name, hash, .. } => (name.clone(), Some(hash.clone())),
        };
        Self {
            backend: SandboxBackend::AndroidMinijail,
            target,
            target_hash,
            no_new_privs: true,
            net_deny: plan.network == NetProfile::DenyNet,
            seccomp_policy: plan
                .seccomp_policy
                .as_ref()
                .map(|s| format!("{}@{}", s.name, s.hash)),
            rlimits: plan
                .rlimits
                .iter()
                .map(|r| format!("{:?}", r.resource))
                .collect(),
            landlock_enforced: false,
            fs_confinement: "app_uid_only".to_string(),
            unsupported_required_features: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ProcessCleanup, Rlimit, RlimitResource, SeccompRef};
    use std::path::PathBuf;

    fn deny_net_system_shell_plan() -> AndroidSandboxPlan {
        AndroidSandboxPlan {
            target: ExecTarget::SystemShell,
            argv: vec!["sh".into(), "-c".into(), "true".into()],
            env: vec![],
            network: NetProfile::DenyNet,
            rlimits: vec![
                Rlimit {
                    resource: RlimitResource::Cpu,
                    soft: 30,
                    hard: 30,
                },
                Rlimit {
                    resource: RlimitResource::Core,
                    soft: 0,
                    hard: 0,
                },
            ],
            seccomp_policy: Some(SeccompRef {
                name: "net-deny-v1".into(),
                hash: "deadbeef".into(),
            }),
            cleanup: ProcessCleanup::KillProcessGroup,
        }
    }

    #[test]
    fn from_plan_deny_net_system_shell() {
        let r = AndroidSandboxReceipt::from_plan(&deny_net_system_shell_plan());
        assert_eq!(r.backend, SandboxBackend::AndroidMinijail);
        assert_eq!(r.target, "SystemShell");
        assert!(r.target_hash.is_none());
        assert!(r.no_new_privs);
        assert!(r.net_deny);
        assert_eq!(r.seccomp_policy.as_deref(), Some("net-deny-v1@deadbeef"));
        // Rlimits recorded by resource name.
        assert!(r.rlimits.iter().any(|s| s.contains("Cpu")));
        assert!(r.rlimits.iter().any(|s| s.contains("Core")));
        // Honest fs reality: no Landlock in v1.
        assert!(!r.landlock_enforced);
        assert_eq!(r.fs_confinement, "app_uid_only");
        assert!(r.unsupported_required_features.is_empty());
    }

    #[test]
    fn from_plan_allow_net_bundled_helper() {
        let plan = AndroidSandboxPlan {
            target: ExecTarget::BundledHelper {
                name: "git".into(),
                path: PathBuf::from("/data/app/x/lib/arm64/libgit.so"),
                hash: "abc123".into(),
            },
            argv: vec!["git".into(), "status".into()],
            env: vec![],
            network: NetProfile::AllowNet,
            rlimits: vec![],
            seccomp_policy: None,
            cleanup: ProcessCleanup::KillProcessGroup,
        };
        let r = AndroidSandboxReceipt::from_plan(&plan);
        assert_eq!(r.target, "git");
        assert_eq!(r.target_hash.as_deref(), Some("abc123"));
        assert!(!r.net_deny, "AllowNet plan installs no net-deny filter");
        assert!(r.seccomp_policy.is_none());
        assert!(!r.landlock_enforced);
    }
}
