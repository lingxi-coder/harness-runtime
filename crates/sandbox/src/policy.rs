//! Sandbox policy defaults.
//!
//! Re-exports the trait-level types and provides a conservative
//! [`default_policy`] used by the [`crate::decision::should_use_sandbox`]
//! decision when no explicit policy is supplied.
//!
//! See spec §24.2 (`SandboxPolicy`).

use mobile_linux_api::{NetworkPolicy, ResourceLimits};
pub use platform_api::SandboxPolicy;

/// Conservative default: no network, project workspace writable,
/// system paths denied, subprocess allowed, modest resource ceilings.
///
/// Plugged in by [`crate::decision::should_use_sandbox`] whenever the
/// decision path decides to sandbox a command but the caller did not
/// provide a custom policy.
#[must_use]
pub fn default_policy(workspace: std::path::PathBuf) -> SandboxPolicy {
    SandboxPolicy {
        network: NetworkPolicy::Disabled,
        writable_paths: vec![workspace],
        denied_paths: vec!["/etc".into(), "/var".into(), "/sys".into(), "/proc".into()],
        allow_subprocess: true,
        limits: ResourceLimits {
            max_cpu_seconds: Some(300),
            max_memory_mb: Some(2048),
            max_processes: Some(50),
            max_open_files: Some(1024),
        },
    }
}
