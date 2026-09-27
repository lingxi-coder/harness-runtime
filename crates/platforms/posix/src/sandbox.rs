//! Real POSIX `Sandbox` implementation.
//!
//! Linux / WSL2 → bwrap via [`sandbox::wrap_with_sandbox`].
//! macOS → `sandbox-exec -f` with an SBPL profile written to a tempfile.
//! WSL1 / unknown POSIX → `is_available()` returns `false`, `prepare()` falls
//! back to a `Wrapped { backend: None }` no-op so callers that ignore the
//! capability flag still get a valid `SandboxedCommand` shape.
//!
//! Spec §6.4 (M2 Plan 04 — `docs/superpowers/plans/2026-05-23-m2-04-sandbox-runtime.md`)
//! is the source-of-truth for the dependency check ordering, the WSL1 refusal
//! string, and the `bwrap` / `sandbox-exec` argv shape.

use async_trait::async_trait;
use platform_api::{
    NetworkPolicy, ProcessCommand, Sandbox, SandboxBackend, SandboxCapability, SandboxError,
    SandboxFeatures, SandboxPolicy, SandboxedCommand, SandboxedTag,
};
use sandbox::dependency_check::{
    check_dependencies, sandbox_unavailable_reason, SandboxDependencyCheck,
};
use sandbox::runtime_config::{
    FilesystemRestrictionConfig, NetworkRestrictionConfig, Platform, SandboxRuntimeConfig,
};
use sandbox::wrap::wrap_with_sandbox;

use crate::wsl_detect::{detect as detect_wsl, WslKind};

/// Real `Sandbox` impl over `bwrap` (Linux/WSL2) / `sandbox-exec` (macOS).
///
/// Construction is cheap (no I/O); the dependency probe happens lazily on
/// [`Sandbox::is_available`] / [`Sandbox::probe_capability`] / [`Sandbox::prepare`].
#[derive(Default)]
pub struct PosixSandbox;

impl PosixSandbox {
    /// Construct a new `PosixSandbox`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build the full dependency check result for this host.
    fn dep_check() -> SandboxDependencyCheck {
        // `in_enabled_list` defaults to true at this layer; the
        // `enabledPlatforms` setting is read at the call-site that has access
        // to the merged settings (M2-04 ships the helper; consumers wire it
        // through when they have settings in hand).
        check_dependencies(detect_platform(), true)
    }

    /// Surface the human-readable unavailable reason for the current host
    /// (or `None` when the sandbox can actually run).
    ///
    /// Faithful to claude-code `getSandboxUnavailableReason` (sandbox-adapter.ts:562):
    /// `enabled` is `sandbox.enabled` (returns `None` when off, so missing deps on
    /// a host where the user never opted in are silent), and `in_enabled_list` is
    /// `isPlatformInEnabledList()` — whether the current platform is in
    /// `sandbox.enabledPlatforms` (computed by the composition root, which holds
    /// the merged settings; this crate stays OS-agnostic about that list).
    #[must_use]
    pub fn unavailable_reason_for(enabled: bool, in_enabled_list: bool) -> Option<String> {
        let wsl_one = matches!(detect_wsl(), WslKind::WslOne);
        let platform = detect_platform();
        let supported = platform.is_some() && !wsl_one;
        let label = if platform.is_none() {
            Some(host_platform_label())
        } else {
            None
        };
        let mut deps = Self::dep_check();
        deps.in_enabled_list = in_enabled_list;
        sandbox_unavailable_reason(enabled, supported, platform, wsl_one, label, &deps)
    }

    /// Surface the unavailable reason for an explicitly-enabled sandbox with no
    /// `enabledPlatforms` restriction. Used by the capability probe, which always
    /// wants a message regardless of the user's `sandbox.enabled` setting.
    fn unavailable_reason() -> Option<String> {
        Self::unavailable_reason_for(true, true)
    }
}

/// WSL-aware host platform accessor for composition roots that need to gate
/// `sandbox_available` on the REAL host (not a coarse `cfg!(target_os)` guess).
///
/// Returns the same `Option<Platform>` as the internal [`detect_platform`]:
/// `Some(Mac)` / `Some(Linux)` / `Some(Wsl)` on a supported host, and `None`
/// for WSL1 (refused, like claude-code) or a non-POSIX host. The desktop build
/// must use THIS rather than `cfg!(target_os = "linux") ⇒ Linux`, otherwise on
/// WSL1 it would wrongly report `Linux` and compute `sandbox_available == true`
/// for a host claude-code explicitly refuses.
#[must_use]
pub fn host_platform() -> Option<Platform> {
    detect_platform()
}

/// Detect the host platform per claude-code's `Platform` enum.
/// Returns `None` for WSL1 (refused) or non-POSIX hosts.
///
/// The `Option` return is intentional even when the macOS cfg branch always
/// produces `Some(Platform::Mac)` — the linux branch genuinely returns `None`
/// for WSL1 and the non-POSIX branch always does. We silence
/// `clippy::unnecessary_wraps` to keep the cross-platform signature uniform.
#[allow(clippy::unnecessary_wraps)]
fn detect_platform() -> Option<Platform> {
    #[cfg(target_os = "macos")]
    {
        Some(Platform::Mac)
    }
    #[cfg(target_os = "linux")]
    {
        match detect_wsl() {
            WslKind::WslTwo => Some(Platform::Wsl),
            WslKind::WslOne => None, // refused
            WslKind::NotWsl => Some(Platform::Linux),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Best-effort host platform label for the "unsupported" error string.
fn host_platform_label() -> String {
    #[cfg(target_os = "macos")]
    {
        "macos".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        "linux".to_string()
    }
    #[cfg(target_os = "windows")]
    {
        "windows".to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        std::env::consts::OS.to_string()
    }
}

#[async_trait]
impl Sandbox for PosixSandbox {
    fn is_available(&self) -> bool {
        if detect_platform().is_none() {
            return false;
        }
        Self::dep_check().errors.is_empty()
    }

    fn backend(&self) -> SandboxBackend {
        match detect_platform() {
            Some(Platform::Mac) => SandboxBackend::MacOsSandboxExec,
            Some(Platform::Linux | Platform::Wsl) => SandboxBackend::LinuxNamespaces,
            None => SandboxBackend::None,
        }
    }

    fn prepare(
        &self,
        cmd: ProcessCommand,
        policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError> {
        // Validate cwd not inside denied paths (preserves M1 behavior for
        // callers that don't touch SandboxRuntimeConfig yet).
        if let Some(cwd) = &cmd.cwd {
            for denied in &policy.denied_paths {
                if cwd.starts_with(denied) {
                    return Err(SandboxError::SymlinkEscape(cwd.display().to_string()));
                }
            }
        }

        // If sandbox is not available on this host, return a Wrapped(None) tag
        // so the SandboxedCommand newtype invariant holds. Callers that pass
        // `failIfUnavailable: true` should consult `is_available()` first.
        let Some(platform) = detect_platform() else {
            return Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::Wrapped {
                    backend: SandboxBackend::None,
                },
            ));
        };
        if !Self::dep_check().errors.is_empty() {
            return Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::Wrapped {
                    backend: SandboxBackend::None,
                },
            ));
        }

        // Translate M1 `SandboxPolicy` → `SandboxRuntimeConfig`.
        let mut runtime_cfg = runtime_config_from_policy(policy);

        // Deny-write by FS existence (finding 3). The bare-repo escape-defense
        // set + the existing generic denied paths re-mount read-only IN PLACE
        // for paths that exist now; absent bare-repo files go to the scrub list
        // (wired into the wrap suffix in Task 5 — inert until then). This port's
        // `prepare` carries one cwd, so `original_cwd == cwd` (the TS
        // `[originalCwd, cwd]` widening collapses to a single dir).
        if !runtime_cfg.filesystem.disabled {
            if let Some(cwd) = &cmd.cwd {
                let (ro_in_place, scrub) = split_bare_repo_paths(std::slice::from_ref(cwd));
                let mut ro = ro_in_place;
                // The wrapper ignored `deny_write` before; now it enforces it for
                // existing paths (absent ones are simply not present to write to).
                for d in &runtime_cfg.filesystem.deny_write {
                    if std::path::Path::new(d).exists() {
                        ro.push(d.clone());
                    }
                }
                runtime_cfg.ro_bind_in_place = ro;
                runtime_cfg.scrub_paths = scrub;
            }
        }

        // Build the full original command string for wrapping (command + args).
        let mut cmd_string = cmd.command.clone();
        for arg in &cmd.args {
            cmd_string.push(' ');
            cmd_string.push_str(arg);
        }

        let wrapped = wrap_with_sandbox(&cmd_string, &runtime_cfg, platform)
            .map_err(|e| SandboxError::Unavailable(format!("wrap_with_sandbox failed: {e}")))?;

        let inner = ProcessCommand {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), wrapped],
            cwd: cmd.cwd,
            env: cmd.env,
            timeout: cmd.timeout,
            stdin: cmd.stdin,
        };

        Ok(SandboxedCommand::__new_sandboxed(
            inner,
            SandboxedTag::Wrapped {
                backend: self.backend(),
            },
        ))
    }

    fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
        tracing::warn!(reason, "sandbox bypass via bypass_with_audit");
        SandboxedCommand::__new_sandboxed(
            cmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.to_string(),
            },
        )
    }

    async fn probe_capability(&self) -> SandboxCapability {
        let available = self.is_available();
        let reason = if available {
            None
        } else {
            // `sandbox_unavailable_reason` returns `None` when `enabled` is
            // false, but we want to surface the reason even when the caller
            // hasn't enabled sandbox yet — they're asking the capability probe,
            // not running. Use `enabled = true` to force message generation.
            Self::unavailable_reason()
        };
        let features = SandboxFeatures {
            network_isolation: available,
            fs_readonly: available,
            fs_readwrite_paths: available,
            process_limit: false, // bwrap can't enforce per-policy.limits today
            no_new_privileges: available,
        };
        SandboxCapability {
            available,
            reason,
            features,
        }
    }
}

/// claude-code bare-repo escape-defense file set (sandbox-adapter.ts:267).
const BARE_GIT_REPO_FILES: [&str; 5] = ["HEAD", "objects", "refs", "hooks", "config"];

/// Split the bare-repo escape-defense paths under each dir by FS existence:
/// existing → ro-bind-in-place (deny write); absent → scrub list (delete
/// post-command). Mirrors sandbox-adapter.ts:264-280. `original_cwd == cwd`
/// in this port (single `prepare` cwd), so the TS `[originalCwd, cwd]` widening
/// collapses to the one dir.
fn split_bare_repo_paths(dirs: &[std::path::PathBuf]) -> (Vec<String>, Vec<String>) {
    let mut ro_in_place = Vec::new();
    let mut scrub = Vec::new();
    for dir in dirs {
        for f in BARE_GIT_REPO_FILES {
            let p = dir.join(f);
            let s = p.to_string_lossy().into_owned();
            if p.exists() {
                ro_in_place.push(s);
            } else {
                scrub.push(s);
            }
        }
    }
    (ro_in_place, scrub)
}

/// Translate the M1 [`SandboxPolicy`] into a [`SandboxRuntimeConfig`] for the
/// wrap dispatcher. The mapping is intentionally narrow — M1 callers only
/// carry `writable_paths`, `denied_paths`, `network`, and `limits`.
///
/// Future work (Task TODO in M2-followup): expand [`SandboxPolicy`] itself to
/// hold a [`SandboxRuntimeConfig`] field so this translation becomes an
/// identity pass and the existing call sites pick up `excludedCommands` etc.
fn runtime_config_from_policy(policy: &SandboxPolicy) -> SandboxRuntimeConfig {
    let allow_write: Vec<String> = policy
        .writable_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let deny_write: Vec<String> = policy
        .denied_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    SandboxRuntimeConfig {
        enabled: true,
        filesystem: FilesystemRestrictionConfig {
            allow_write,
            deny_write,
            ..Default::default()
        },
        network: NetworkRestrictionConfig {
            // Conservative: only full-allow requests external egress. LoopbackOnly
            // is satisfied by bwrap's fresh netns (loopback present, external
            // blocked) → empty allowed_domains → `--unshare-net` in the wrapper.
            allow_local_binding: matches!(policy.network, NetworkPolicy::LoopbackOnly),
            allowed_domains: if matches!(policy.network, NetworkPolicy::Allowed) {
                vec!["*".to_string()]
            } else {
                vec![]
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{runtime_config_from_policy, split_bare_repo_paths};
    use platform_api::{NetworkPolicy, ResourceLimits, SandboxPolicy};

    /// Build a minimal `SandboxPolicy` literal for net-mapping tests.
    /// `SandboxPolicy` does not derive `Default`, so construct each field.
    fn policy_with_network(network: NetworkPolicy) -> SandboxPolicy {
        SandboxPolicy {
            network,
            writable_paths: vec![],
            denied_paths: vec![],
            allow_subprocess: false,
            limits: ResourceLimits::default(),
        }
    }

    #[test]
    fn loopback_and_disabled_do_not_request_full_egress() {
        let mut p = policy_with_network(NetworkPolicy::LoopbackOnly);
        let cfg = runtime_config_from_policy(&p);
        assert!(
            cfg.network.allowed_domains.is_empty(),
            "loopback must not map to [*]"
        );
        p.network = NetworkPolicy::Disabled;
        assert!(runtime_config_from_policy(&p)
            .network
            .allowed_domains
            .is_empty());
        p.network = NetworkPolicy::Allowed;
        assert_eq!(
            runtime_config_from_policy(&p).network.allowed_domains,
            vec!["*".to_string()]
        );
    }

    #[test]
    fn existing_denied_paths_go_ro_in_place_absent_go_scrub() {
        let tmp = tempfile::tempdir().unwrap();
        let exists = tmp.path().join("HEAD");
        std::fs::write(&exists, "x").unwrap();
        let absent = tmp.path().join("objects");
        let (ro, scrub) = split_bare_repo_paths(&[tmp.path().to_path_buf()]);
        assert!(ro.contains(&exists.to_string_lossy().into_owned()));
        assert!(scrub.contains(&absent.to_string_lossy().into_owned()));
        assert!(!ro.iter().any(|p| p.ends_with("objects")));
    }
}
