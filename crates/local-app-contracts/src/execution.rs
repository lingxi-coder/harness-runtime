//! Running one command in isolation: what the service asks for, what a host
//! reports it actually did.
//!
//! Building an app runs code the app's workspace pulled in, so the service
//! never takes a host's word for the isolation it asked for: every run comes
//! back with an [`Enforcement`] receipt, and [`Enforcement::ensure_for`]
//! fails closed when the host did not enforce what the command required.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// How much of the network a command may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No outbound network at all.
    Disabled,
    /// Loopback only.
    LoopbackOnly,
    /// Full outbound network access.
    Allowed,
}

/// Ceilings for the commands a build runs. `None` means no ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum CPU time in seconds.
    pub max_cpu_seconds: Option<u32>,
    /// Maximum resident memory in megabytes.
    pub max_memory_mb: Option<u32>,
    /// Maximum number of child processes and threads.
    pub max_processes: Option<u32>,
    /// Maximum number of open file descriptors.
    pub max_open_files: Option<u32>,
}

/// What a mount is for, which decides how a host may treat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    /// The app's project: the one place a build may write.
    Project,
    /// The content-addressable store of downloaded dependencies, shared across
    /// apps and used only while installing them.
    DependencyStore,
}

/// One host directory made visible inside the isolated environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Where the directory is on the host.
    pub host_path: PathBuf,
    /// Where it appears in the isolated environment.
    pub guest_path: String,
    /// Whether the command may not write to it.
    pub read_only: bool,
    /// What it is for.
    pub kind: MountKind,
}

/// One command to run in isolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedCommand {
    /// The program, as a path inside the isolated environment.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The working directory inside the isolated environment.
    pub cwd: Option<String>,
    /// The only environment the program sees.
    pub env: BTreeMap<String, String>,
    /// How long it may run, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// The network it may use.
    pub network: NetworkPolicy,
    /// The resources it may use.
    pub limits: ResourceLimits,
    /// What it can see of the host.
    pub mounts: Vec<Mount>,
}

/// What a host actually enforced for one command. A host must never claim
/// enforcement it did not apply to the whole process group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Enforcement {
    /// The network policy was enforced.
    pub network_policy_enforced: bool,
    /// The memory ceiling was enforced.
    pub memory_limit_enforced: bool,
}

impl Enforcement {
    /// Fail unless the host enforced everything a command required. `Allowed`
    /// needs no network proof; every stricter policy and every memory ceiling
    /// does. The messages are the ones a build has always reported.
    pub fn ensure_for(self, network: NetworkPolicy, limits: ResourceLimits) -> Result<(), String> {
        if !matches!(network, NetworkPolicy::Allowed) && !self.network_policy_enforced {
            return Err(format!(
                "network_policy_unavailable: backend did not enforce requested {network:?} network policy"
            ));
        }
        if limits.max_memory_mb.is_some() && !self.memory_limit_enforced {
            return Err(
                "resource_limit_exceeded: backend did not enforce requested resident-memory limit"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// What one command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    /// Everything it wrote to standard output.
    pub stdout: String,
    /// Everything it wrote to standard error.
    pub stderr: String,
    /// Its exit code.
    pub exit_code: i32,
    /// Whether it was killed for running too long.
    pub timed_out: bool,
    /// Whether it was cancelled before it finished.
    pub cancelled: bool,
    /// What the host enforced while it ran.
    pub enforcement: Enforcement,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(memory: Option<u32>) -> ResourceLimits {
        ResourceLimits {
            max_memory_mb: memory,
            ..ResourceLimits::default()
        }
    }

    #[test]
    fn an_unenforced_network_policy_fails_unless_the_network_is_allowed() {
        let nothing = Enforcement::default();
        assert!(nothing
            .ensure_for(NetworkPolicy::Allowed, limits(None))
            .is_ok());
        for stricter in [NetworkPolicy::Disabled, NetworkPolicy::LoopbackOnly] {
            let error = nothing.ensure_for(stricter, limits(None)).unwrap_err();
            assert!(
                error.starts_with("network_policy_unavailable: ")
                    && error.contains(&format!("{stricter:?}")),
                "{error}"
            );
        }
        let enforced = Enforcement {
            network_policy_enforced: true,
            memory_limit_enforced: false,
        };
        assert!(enforced
            .ensure_for(NetworkPolicy::Disabled, limits(None))
            .is_ok());
    }

    #[test]
    fn a_memory_ceiling_needs_proof_and_no_ceiling_needs_none() {
        let network_only = Enforcement {
            network_policy_enforced: true,
            memory_limit_enforced: false,
        };
        assert_eq!(
            network_only
                .ensure_for(NetworkPolicy::Disabled, limits(Some(512)))
                .unwrap_err(),
            "resource_limit_exceeded: backend did not enforce requested resident-memory limit"
        );
        let both = Enforcement {
            network_policy_enforced: true,
            memory_limit_enforced: true,
        };
        assert!(both
            .ensure_for(NetworkPolicy::Disabled, limits(Some(512)))
            .is_ok());
        // Other ceilings are not part of the receipt.
        let cpu_only = ResourceLimits {
            max_cpu_seconds: Some(5),
            ..ResourceLimits::default()
        };
        assert!(network_only
            .ensure_for(NetworkPolicy::Disabled, cpu_only)
            .is_ok());
    }
}
