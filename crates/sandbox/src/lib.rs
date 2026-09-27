//! Sandbox decision engine + reusable policy helpers.
//!
//! `lingxi-sandbox` does not ship a backend (those live in platform crates).
//! It provides:
//!
//! - [`policy::default_policy`] — the conservative policy used when callers
//!   don't supply one explicitly.
//! - [`decision::should_use_sandbox`] — the faithful `shouldUseSandbox` port the
//!   tool layer uses to pick between sandbox / no-sandbox (gates on host
//!   availability, the `dangerouslyDisableSandbox` override, an empty command,
//!   and the `excludedCommands` list — no permission-mode / trust / classifier).
//! - [`decision::should_use_sandbox_for_command`] — the
//!   `SandboxRuntimeConfig`-aware `enabled && !excluded` predicate sharing the
//!   same excluded-command core (port of claude-code's `shouldUseSandbox.ts`).
//! - [`dependency_check::check_dependencies`] — host probe for required
//!   sandbox binaries (`sandbox-exec`, `bwrap`, `socat`).
//! - [`dependency_check::sandbox_unavailable_reason`] — decoder for the
//!   five claude-code byte-for-byte error strings surfaced when sandbox
//!   can't run.
//! - [`violation_store::SandboxViolationStore`] — bounded ring buffer of
//!   sandbox violation events surfaced by the backend.
//! - [`canonicalize_safely`] — a symlink-escape-safe path canonicalization
//!   helper used by backend impls.
//!
//! See spec §24 (Sandbox).

#![forbid(unsafe_code)]

pub mod credentials;
pub mod decision;
pub mod dependency_check;
pub mod path_pattern;
pub mod policy;
pub mod policy_convert;
pub mod runtime_config;
pub mod violation_store;
pub mod wrap;

pub use decision::{
    is_binary_hijack_var, should_use_sandbox, should_use_sandbox_for_command, SandboxDecision,
};
pub use dependency_check::{
    check_dependencies, sandbox_unavailable_reason, MissingDeps, SandboxDependencyCheck,
};
pub use path_pattern::resolve_path_pattern_for_sandbox;
pub use permission::shell_command::strip_env_and_wrappers_fixedpoint;
pub use platform_api::{
    NetworkPolicy, ResourceLimits, Sandbox, SandboxBackend, SandboxError, SandboxPolicy,
    SandboxedCommand, SandboxedTag,
};
pub use policy::default_policy;
pub use policy_convert::{convert_settings_to_runtime_config, linux_glob_pattern_warnings};
pub use runtime_config::{
    FilesystemRestrictionConfig, NetworkRestrictionConfig, Platform, RipgrepConfig,
    SandboxRuntimeConfig, SandboxSettingsJson, SettingsJson, SettingsPermissions,
};
pub use violation_store::{
    SandboxViolationEvent, SandboxViolationKind, SandboxViolationStore, SANDBOX_VIOLATION_STORE_CAP,
};
pub use wrap::{wrap_with_sandbox, SandboxWrapError};

/// Canonicalize `path` and verify the result stays inside `workspace`.
///
/// Used by platform sandbox impls (spec A2) before mounting writable paths
/// into a sandbox: the kernel resolves symlinks at `realpath` time, so any
/// link that escapes the workspace would otherwise allow a sandboxed
/// process to write outside the project.
///
/// # Errors
/// Returns [`platform_api::SandboxError::PathCanonicalize`] if the kernel
/// canonicalization itself fails, or
/// [`platform_api::SandboxError::SymlinkEscape`] when the canonical path
/// resolves outside `workspace`.
pub fn canonicalize_safely(
    path: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<std::path::PathBuf, platform_api::SandboxError> {
    let canon = path
        .canonicalize()
        .map_err(|e| platform_api::SandboxError::PathCanonicalize(e.to_string()))?;
    if !canon.starts_with(workspace) {
        return Err(platform_api::SandboxError::SymlinkEscape(
            path.display().to_string(),
        ));
    }
    Ok(canon)
}
