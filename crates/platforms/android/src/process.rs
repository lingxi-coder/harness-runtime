//! [`lingxi_core::host::ProcessRunner`] for Android. P2 turns on execution: after the P1
//! security invariants pass, the runner translates the prepared
//! [`AndroidSandboxPlan`] into a [`platform_android_minijail::JailSpec`] and
//! runs it through `run_jailed` on a blocking thread. `platform-android` stays
//! `forbid(unsafe_code)` — all FFI lives in `platform-android-minijail`.

use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::host::{
    ProcessError, ProcessHandle, ProcessOutput, ProcessRunner, SandboxBackend, SandboxedCommand,
    SandboxedTag,
};
use platform_android_minijail::{JailRlimit, JailSpec};

use crate::capabilities::CapabilityCache;
use crate::policy::{AndroidSandboxPlan, NetProfile, RlimitResource};
use crate::receipt::AndroidSandboxReceipt;

/// Default wall-clock budget when the command carries no explicit timeout
/// (mirrors the desktop shell's default; the watchdog kills the group past it).
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Android process runner. Holds the shared capability cache so per-plan
/// admission (e.g. `DenyNet` requiring a working net-deny seccomp filter) reads
/// the SAME probed result the sandbox/registration gate read.
pub struct AndroidMinijailProcessRunner {
    caps: Arc<CapabilityCache>,
}

/// Map a [`RlimitResource`] to its raw `RLIMIT_*` integer.
///
/// These are the stable Linux/Android ABI numbers from
/// `<asm-generic/resource.h>` (identical across `arm64`/`x86_64`): `RLIMIT_CPU=0`,
/// `RLIMIT_FSIZE=1`, `RLIMIT_CORE=4`, `RLIMIT_NOFILE=7`, `RLIMIT_AS=9`. Hardcoded
/// here so `platform-android` stays libc-free (and `forbid(unsafe_code)`); the
/// runner passes the int to `minijail_rlimit` via the spec.
#[must_use]
fn rlimit_resource_int(r: RlimitResource) -> i32 {
    match r {
        RlimitResource::Cpu => 0,    // RLIMIT_CPU
        RlimitResource::Core => 4,   // RLIMIT_CORE
        RlimitResource::NoFile => 7, // RLIMIT_NOFILE
        RlimitResource::As => 9,     // RLIMIT_AS
    }
}

/// Verify a bundled helper on disk matches its recorded content hash before
/// we hand its path to execve — defense against a swapped/tampered binary.
fn verify_bundled_identity(
    name: &str,
    path: &std::path::Path,
    expected_hex: &str,
) -> Result<(), ProcessError> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| {
        ProcessError::SandboxEnforcementFailed(format!(
            "bundled helper {name} unreadable for identity check ({}): {e}",
            path.display()
        ))
    })?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(ProcessError::SandboxEnforcementFailed(format!(
            "bundled helper {name} hash mismatch (identity check failed)"
        )));
    }
    Ok(())
}

impl AndroidMinijailProcessRunner {
    /// Construct over the shared capability cache.
    #[must_use]
    pub fn new(caps: Arc<CapabilityCache>) -> Self {
        Self { caps }
    }

    /// Translate the admitted plan + the audit-mirror inner command into the
    /// FFI boundary [`JailSpec`]. `plan.argv`/`plan.env` are authoritative; the
    /// cwd and timeout come from the inner command (canonicalized in prepare).
    fn build_spec(plan: &AndroidSandboxPlan, sandboxed: &SandboxedCommand) -> JailSpec {
        let inner = sandboxed.inner();
        let net_deny = plan.network == NetProfile::DenyNet;
        let bpf = if net_deny {
            // Per-arch socket nrs + AUDIT_ARCH resolved on-device; empty on host
            // (the host run_jailed reports enforcement failure before BPF use).
            platform_android_minijail::net_deny_bpf_for_target()
        } else {
            Vec::new()
        };
        let rlimits = plan
            .rlimits
            .iter()
            .map(|r| JailRlimit {
                resource: rlimit_resource_int(r.resource),
                soft: r.soft,
                hard: r.hard,
            })
            .collect();
        let cwd = inner
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let timeout_ms = inner.timeout.map_or(DEFAULT_TIMEOUT_MS, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        });
        // The runner execs the plan's target: a BundledHelper runs its packaged
        // path (P2's ExecTarget seam — P5's mksh is the first real consumer); a
        // SystemShell runs the inner audit-mirror command (/system/bin/sh).
        let filename = match &plan.target {
            crate::policy::ExecTarget::BundledHelper { path, .. } => path.display().to_string(),
            crate::policy::ExecTarget::SystemShell => inner.command.clone(),
        };
        JailSpec {
            filename,
            argv: plan.argv.clone(),
            envp: plan.env.clone(),
            cwd,
            rlimits,
            net_deny,
            bpf,
            timeout_ms,
        }
    }

    /// Validate the three security invariants and return the admitted plan.
    ///
    /// # Why the runner re-checks what the type system cannot
    ///
    /// `__new_sandboxed_with_plan` does not constrain tag↔plan consistency:
    /// any caller can attach any plan to any tag (or no plan). The runner is
    /// therefore the last-chance enforcement point (defense-in-depth per spec
    /// r3 §`AndroidMinijailProcessRunner` and prior review):
    ///
    /// 1. `BypassAuditedWithReason` — the desktop `BashTool` shape dies here
    ///    by construction; this is the ONLY audit trace if it was silently
    ///    dropped upstream.
    /// 2. `Wrapped` with a backend ≠ `AndroidMinijail` — a foreign plan
    ///    (e.g. macOS `sandbox-exec`) must never execute on Android.
    /// 3. Plan missing or not downcasting to `AndroidSandboxPlan` — the plan
    ///    field is `Option` so `prepare()` could have been bypassed; downcast
    ///    failure means the wrong backend minted the handle.
    fn admitted_plan(cmd: &SandboxedCommand) -> Result<Arc<AndroidSandboxPlan>, ProcessError> {
        match cmd.tag() {
            SandboxedTag::BypassAuditedWithReason { reason } => {
                let msg =
                    "bypass-audited commands cannot execute on Android (security invariant #1)";
                tracing::warn!(bypass_reason = %reason, "android runner rejected bypass-audited command");
                return Err(ProcessError::MalformedSandboxPlan(msg.into()));
            }
            SandboxedTag::Wrapped { backend } if *backend != SandboxBackend::AndroidMinijail => {
                let msg = format!("foreign sandbox backend {backend:?} (security invariant #2)");
                tracing::warn!(rejection = %msg, "android runner rejected command");
                return Err(ProcessError::MalformedSandboxPlan(msg));
            }
            SandboxedTag::Wrapped { .. } => {}
        }
        let handle = cmd.backend_plan().ok_or_else(|| {
            let msg = "android plan missing from SandboxedCommand (security invariant #3)";
            tracing::warn!(rejection = msg, "android runner rejected command");
            ProcessError::MalformedSandboxPlan(msg.into())
        })?;
        handle.downcast::<AndroidSandboxPlan>().ok_or_else(|| {
            let msg = "backend plan failed to downcast to AndroidSandboxPlan (security invariant #3) — wrong backend minted the handle, or the plan was wrapped pre-Arc'd";
            tracing::warn!(rejection = msg, "android runner rejected command");
            ProcessError::MalformedSandboxPlan(msg.into())
        })
    }
}

#[async_trait]
impl ProcessRunner for AndroidMinijailProcessRunner {
    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        // (0) Security invariants first (P1) — execution only past these.
        // plan.argv/plan.env are authoritative; cmd.inner() mirrors them for audit.
        let plan = Self::admitted_plan(cmd)?;

        // (1) Per-plan capability gate (spec sandbox.rs P2 NOTE): a DenyNet plan
        // MUST NOT run unless the device proved it can install a net-deny seccomp
        // filter AND observed socket() == EPERM under it. Otherwise we'd run a
        // shell with full network access while claiming deny-net — fail closed.
        let caps = self.caps.get();
        if plan.network == NetProfile::DenyNet && !(caps.seccomp_filter && caps.net_deny_verified) {
            return Err(ProcessError::PolicyUnsupported(
                "net-deny seccomp filter unavailable; cannot honor deny-net".into(),
            ));
        }

        // (1b) Identity-check a bundled helper before its path reaches execve:
        // canonical-path bytes must match the recorded content hash, else a
        // swapped/tampered binary could be run. Fail-closed (read error OR
        // mismatch → error, never exec). SystemShell carries no hash.
        if let crate::policy::ExecTarget::BundledHelper { path, hash, name } = &plan.target {
            verify_bundled_identity(name, path, hash)?;
        }

        // (2) Translate the plan + inner command into the FFI boundary spec, run
        // it on a blocking thread (the jailed fork/exec + watchdog is blocking).
        let spec = Self::build_spec(&plan, cmd);
        let out = tokio::task::spawn_blocking(move || platform_android_minijail::run_jailed(&spec))
            .await
            .map_err(|e| ProcessError::Io(format!("jailed run task panicked/cancelled: {e}")))?;

        // (3) A setup failure (filter load, rlimit, fork, host-build stub) maps to
        // a structured enforcement error — NEVER a silent unconfined run.
        if let Some(reason) = out.enforcement_failed {
            return Err(ProcessError::SandboxEnforcementFailed(reason));
        }

        // (4) Honest receipt for the process log / tool meta.
        let receipt = AndroidSandboxReceipt::from_plan(&plan);
        tracing::info!(
            backend = ?receipt.backend,
            target = %receipt.target,
            no_new_privs = receipt.no_new_privs,
            net_deny = receipt.net_deny,
            seccomp_policy = ?receipt.seccomp_policy,
            rlimits = ?receipt.rlimits,
            landlock_enforced = receipt.landlock_enforced,
            fs_confinement = %receipt.fs_confinement,
            "android jailed run completed"
        );

        Ok(ProcessOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
            timed_out: out.timed_out,
        })
    }

    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        // Invariants must hold even for background tasks — bypass/foreign-backend
        // commands are rejected here, producing an audit trace (MalformedSandboxPlan)
        // rather than silently returning Unsupported.
        let _plan = Self::admitted_plan(cmd)?;
        // Spec non-goal 5: no background shell tasks in v1.
        Err(ProcessError::Unsupported)
    }

    async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        Err(ProcessError::Unsupported)
    }

    fn is_available(&self) -> bool {
        // Reads the shared probed cache (was hard-false in P1). The coarse gate
        // (probed + smoke + no_new_privs); per-plan requirements like the
        // net-deny filter are enforced in `run()`.
        self.caps.get().available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::{
        BackendPlanHandle, ProcessCommand, ProcessError, ProcessRunner, SandboxBackend,
        SandboxedCommand, SandboxedTag,
    };
    use std::collections::HashMap;

    fn cmd() -> ProcessCommand {
        ProcessCommand {
            command: "/system/bin/sh".into(),
            args: vec![],
            cwd: None,
            env: HashMap::new(),
            timeout: None,
            stdin: None,
        }
    }

    use crate::capabilities::{AndroidSandboxCapabilities, CapabilityCache};

    /// A fully-capable probed cache: smoke + nnp + `seccomp_filter` +
    /// `net_deny_verified` all true — admits a `DenyNet` plan through to
    /// `run_jailed`.
    fn ready_cache() -> Arc<CapabilityCache> {
        let cache = CapabilityCache::new();
        cache.set(AndroidSandboxCapabilities {
            probed: true,
            minijail_smoke: true,
            no_new_privs: true,
            seccomp_filter: true,
            net_deny_verified: true,
            ..AndroidSandboxCapabilities::default()
        });
        Arc::new(cache)
    }

    fn runner() -> AndroidMinijailProcessRunner {
        AndroidMinijailProcessRunner::new(ready_cache())
    }

    fn valid_plan() -> crate::policy::AndroidSandboxPlan {
        deny_net_plan_with_seccomp()
    }

    /// A `DenyNet` `AndroidSandboxPlan` carrying its net-deny `SeccompRef`,
    /// mirroring what `plan_from_policy` mints for a deny-net shell.
    fn deny_net_plan_with_seccomp() -> crate::policy::AndroidSandboxPlan {
        crate::policy::AndroidSandboxPlan {
            target: crate::policy::ExecTarget::SystemShell,
            argv: vec!["sh".into(), "-c".into(), "true".into()],
            env: vec![],
            network: crate::policy::NetProfile::DenyNet,
            rlimits: vec![crate::policy::Rlimit {
                resource: crate::policy::RlimitResource::Core,
                soft: 0,
                hard: 0,
            }],
            seccomp_policy: Some(crate::policy::SeccompRef {
                name: "net-deny-v1".into(),
                hash: "id".into(),
            }),
            cleanup: crate::policy::ProcessCleanup::KillProcessGroup,
        }
    }

    /// Wrap a plan in an `AndroidMinijail`-tagged `SandboxedCommand` (mirrors the
    /// P1 helper shape: a `Wrapped{AndroidMinijail}` tag + the attached plan).
    fn sandboxed_with_plan(plan: crate::policy::AndroidSandboxPlan) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed_with_plan(
            cmd(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidMinijail,
            },
            BackendPlanHandle::new(plan),
        )
    }

    #[tokio::test]
    async fn rejects_bypass_audited_commands() {
        let sc = SandboxedCommand::__new_sandboxed(
            cmd(),
            SandboxedTag::BypassAuditedWithReason {
                reason: "bash_tool_call".into(),
            },
        );
        let err = runner().run(&sc).await.unwrap_err();
        assert!(matches!(err, ProcessError::MalformedSandboxPlan(ref m)
            if m.contains("bypass")));
    }

    #[tokio::test]
    async fn rejects_foreign_backend() {
        let sc = SandboxedCommand::__new_sandboxed(
            cmd(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::MacOsSandboxExec,
            },
        );
        let err = runner().run(&sc).await.unwrap_err();
        assert!(matches!(err, ProcessError::MalformedSandboxPlan(ref m)
            if m.contains("backend")));
    }

    #[tokio::test]
    async fn rejects_missing_or_foreign_plan() {
        let missing = SandboxedCommand::__new_sandboxed(
            cmd(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidMinijail,
            },
        );
        let err = runner().run(&missing).await.unwrap_err();
        assert!(matches!(err, ProcessError::MalformedSandboxPlan(ref m)
            if m.contains("missing")));

        let foreign = SandboxedCommand::__new_sandboxed_with_plan(
            cmd(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidMinijail,
            },
            BackendPlanHandle::new(String::from("not a plan")),
        );
        let err = runner().run(&foreign).await.unwrap_err();
        assert!(matches!(err, ProcessError::MalformedSandboxPlan(ref m)
            if m.contains("downcast")));
    }

    #[tokio::test]
    async fn host_run_maps_enforcement_failure_to_structured_error() {
        // A valid AndroidMinijail plan over a fully-capable cache passes the
        // per-plan gate and reaches run_jailed, which on the host reports
        // enforcement failure → SandboxEnforcementFailed (NOT Io/Unsupported).
        let sc = sandboxed_with_plan(deny_net_plan_with_seccomp());
        let err = AndroidMinijailProcessRunner::new(ready_cache())
            .run(&sc)
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::SandboxEnforcementFailed(_)));
    }

    #[tokio::test]
    async fn deny_net_plan_without_filter_capability_fails_closed() {
        // caps say seccomp_filter=false → a DenyNet plan must be refused with a
        // named PolicyUnsupported, never run unconfined.
        let cache = CapabilityCache::new();
        cache.set(AndroidSandboxCapabilities {
            probed: true,
            minijail_smoke: true,
            no_new_privs: true,
            seccomp_filter: false,
            net_deny_verified: false,
            ..AndroidSandboxCapabilities::default()
        });
        let err = AndroidMinijailProcessRunner::new(Arc::new(cache))
            .run(&sandboxed_with_plan(deny_net_plan_with_seccomp()))
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::PolicyUnsupported(ref m) if m.contains("seccomp")));
    }

    #[test]
    fn rlimit_resource_ints_pin_linux_abi() {
        // Stable Linux/Android ABI numbers (<asm-generic/resource.h>).
        assert_eq!(rlimit_resource_int(RlimitResource::Cpu), 0);
        assert_eq!(rlimit_resource_int(RlimitResource::Core), 4);
        assert_eq!(rlimit_resource_int(RlimitResource::NoFile), 7);
        assert_eq!(rlimit_resource_int(RlimitResource::As), 9);
    }

    #[test]
    fn is_available_reads_the_shared_cache() {
        // Unprobed cache → unavailable; a probed+smoke+nnp cache → available.
        let unprobed = AndroidMinijailProcessRunner::new(Arc::new(CapabilityCache::new()));
        assert!(!unprobed.is_available());
        assert!(runner().is_available());
    }

    #[tokio::test]
    async fn bypass_tag_with_valid_plan_attached_still_rejected() {
        let sc = SandboxedCommand::__new_sandboxed_with_plan(
            cmd(),
            SandboxedTag::BypassAuditedWithReason {
                reason: "bash_tool_call".into(),
            },
            BackendPlanHandle::new(valid_plan()),
        );
        let err = runner().run(&sc).await.unwrap_err();
        assert!(matches!(err, ProcessError::MalformedSandboxPlan(ref m) if m.contains("bypass")));
    }

    #[tokio::test]
    async fn foreign_backend_with_valid_plan_attached_still_rejected() {
        let sc = SandboxedCommand::__new_sandboxed_with_plan(
            cmd(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::MacOsSandboxExec,
            },
            BackendPlanHandle::new(valid_plan()),
        );
        let err = runner().run(&sc).await.unwrap_err();
        assert!(matches!(err, ProcessError::MalformedSandboxPlan(ref m) if m.contains("backend")));
    }

    /// Build a `BundledHelper` plan otherwise identical to the deny-net shell
    /// plan, but execing the given bundled path/hash instead of `/system/bin/sh`.
    fn bundled_helper_plan(
        name: &str,
        path: std::path::PathBuf,
        hash: &str,
    ) -> crate::policy::AndroidSandboxPlan {
        crate::policy::AndroidSandboxPlan {
            target: crate::policy::ExecTarget::BundledHelper {
                name: name.into(),
                path,
                hash: hash.into(),
            },
            ..deny_net_plan_with_seccomp()
        }
    }

    #[test]
    fn build_spec_uses_bundled_path_as_filename() {
        // A BundledHelper plan must exec the bundled path, not /system/bin/sh.
        let plan = bundled_helper_plan("mksh", "/nl/libmksh.so".into(), "deadbeef");
        let spec =
            AndroidMinijailProcessRunner::build_spec(&plan, &sandboxed_with_plan(plan.clone()));
        assert_eq!(spec.filename, "/nl/libmksh.so");
        assert_eq!(spec.argv[0], "sh");

        // A SystemShell plan still execs the inner command (/system/bin/sh).
        let sys = deny_net_plan_with_seccomp();
        let sys_spec =
            AndroidMinijailProcessRunner::build_spec(&sys, &sandboxed_with_plan(sys.clone()));
        assert_eq!(sys_spec.filename, "/system/bin/sh");
    }

    #[tokio::test]
    async fn bundled_helper_hash_mismatch_fails_closed() {
        use std::io::Write as _;

        // Known bytes on disk; compute their real sha256 for the control case.
        let bytes = b"#!/system/bin/sh\nexit 0\n";
        let dir = std::env::temp_dir();
        let path = dir.join(format!("p5b-bundled-{}.bin", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).expect("create temp helper");
            f.write_all(bytes).expect("write temp helper");
        }
        let correct_hex = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(bytes))
        };

        // WRONG hash → identity check fires before run_jailed, naming hash/identity.
        let wrong = bundled_helper_plan("mksh", path.clone(), "00deadbeef00");
        let err = runner().run(&sandboxed_with_plan(wrong)).await.unwrap_err();
        match err {
            ProcessError::SandboxEnforcementFailed(ref m) => {
                assert!(
                    m.contains("hash") || m.contains("identity"),
                    "wrong-hash error must name hash/identity, got {m:?}"
                );
            }
            other => panic!("expected SandboxEnforcementFailed for hash mismatch, got {other:?}"),
        }

        // CORRECT hash → passes the identity check, then hits the host run_jailed
        // enforcement stub. Still SandboxEnforcementFailed, but NOT the hash text.
        let good = bundled_helper_plan("mksh", path.clone(), &correct_hex);
        let err = runner().run(&sandboxed_with_plan(good)).await.unwrap_err();
        match err {
            ProcessError::SandboxEnforcementFailed(ref m) => {
                assert!(
                    !m.contains("hash mismatch") && !m.contains("identity check failed"),
                    "correct-hash run must pass identity check, got {m:?}"
                );
            }
            other => panic!("expected SandboxEnforcementFailed from run_jailed, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }
}
