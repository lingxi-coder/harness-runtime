//! `SandboxPolicy` → [`AndroidSandboxPlan`] mapping (spec r3 §Policy mapping).
//!
//! Pure functions, no I/O: validation/planning happens in `sandbox.rs`,
//! execution in `process.rs`. Fail-closed: every unenforceable request is
//! rejected with a named guarantee.

use mobile_linux_api::NetworkPolicy;
use platform_api::{SandboxError, SandboxPolicy};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What the runner will ultimately `execve` (spec r3 §`AndroidSandboxPlan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecTarget {
    /// `/system/bin/sh -c <command>` — the v1 interpreter; becomes the
    /// internal diagnostic fallback after P5.
    SystemShell,
    /// A packaged executable under `nativeLibraryDir` (git at P4, bundled
    /// mksh/toybox at P5). Identity-checked: canonical path + content hash.
    BundledHelper {
        /// Helper short name (`"git"`, `"mksh"`, …).
        name: String,
        /// Canonicalized absolute path under `nativeLibraryDir`.
        path: PathBuf,
        /// Hex SHA-256 of the binary, recorded in capabilities + receipts.
        hash: String,
    },
}

/// Network stance compiled into the jail (spec D10: Shell is `DenyNet` always;
/// only structured tools may build `AllowNet` plans).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetProfile {
    /// seccomp denies socket-family syscalls for the whole process tree.
    DenyNet,
    /// No network restriction (structured Git tool only, P4+).
    AllowNet,
}

/// One rlimit the jail applies before exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rlimit {
    /// Which resource.
    pub resource: RlimitResource,
    /// Soft limit value.
    pub soft: u64,
    /// Hard limit value.
    pub hard: u64,
}

/// The subset of rlimits the spec maps (spec r3 §Policy mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlimitResource {
    /// `RLIMIT_CPU` (seconds).
    Cpu,
    /// `RLIMIT_AS` (bytes, best effort).
    As,
    /// `RLIMIT_NOFILE`.
    NoFile,
    /// `RLIMIT_CORE` (always 0).
    Core,
}

/// Reference to a compiled seccomp filter (name + content hash for receipts).
/// `None` in P1 — the net-deny filter is compiled in P2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeccompRef {
    /// Filter name (`"net-deny-v1"`).
    pub name: String,
    /// Hex SHA-256 of the compiled BPF program.
    pub hash: String,
}

/// How the runner tears the child down on timeout/cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessCleanup {
    /// `setsid()` at spawn; `kill(-pgid, SIGKILL)` on expiry.
    KillProcessGroup,
}

/// The prepared, in-process-only execution plan (never serialized — spec D7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidSandboxPlan {
    /// Exec target.
    pub target: ExecTarget,
    /// Full argv (argv[0] included).
    pub argv: Vec<String>,
    /// Post-scrub environment (the ONLY env the child sees).
    pub env: Vec<(String, String)>,
    /// Network stance.
    pub network: NetProfile,
    /// Rlimits to apply.
    pub rlimits: Vec<Rlimit>,
    /// Compiled seccomp filter reference (P2+; `None` in P1).
    pub seccomp_policy: Option<SeccompRef>,
    /// Teardown strategy.
    pub cleanup: ProcessCleanup,
}

/// Map a [`SandboxPolicy`] onto an [`AndroidSandboxPlan`], failing closed with
/// a **named guarantee** for everything Android cannot enforce
/// (spec r3 §Policy mapping). Pure; argv is filled by the caller.
///
/// # Errors
/// `SandboxError::Unavailable` naming the unenforceable guarantee.
pub fn plan_from_policy(
    target: ExecTarget,
    policy: &SandboxPolicy,
    env: Vec<(String, String)>,
) -> Result<AndroidSandboxPlan, SandboxError> {
    let network = match (policy.network, &target) {
        (NetworkPolicy::LoopbackOnly, _) => {
            return Err(SandboxError::Unavailable(
                "loopback-only network policy is unenforceable on Android (seccomp cannot \
                 inspect sockaddr)"
                    .into(),
            ));
        }
        (NetworkPolicy::Allowed, ExecTarget::SystemShell) => {
            return Err(SandboxError::Unavailable(
                "networked shell is not supported; network grants exist only on structured \
                 tools (spec D10)"
                    .into(),
            ));
        }
        (NetworkPolicy::Allowed, ExecTarget::BundledHelper { .. }) => NetProfile::AllowNet,
        (NetworkPolicy::Disabled, _) => NetProfile::DenyNet,
    };

    if !policy.writable_paths.is_empty() || !policy.denied_paths.is_empty() {
        return Err(SandboxError::Unavailable(
            "filesystem confinement requires Landlock, which shipping Android kernels do not \
             enable; the default mobile policy must request none"
                .into(),
        ));
    }

    if !policy.allow_subprocess {
        return Err(SandboxError::Unavailable(
            "subprocess denial is not enforceable for shell/git targets on Android (both \
             require children)"
                .into(),
        ));
    }

    let mut rlimits = Vec::new();
    if let Some(cpu) = policy.limits.max_cpu_seconds {
        rlimits.push(Rlimit {
            resource: RlimitResource::Cpu,
            soft: u64::from(cpu),
            hard: u64::from(cpu),
        });
    }
    // limits.max_memory_mb is intentionally NOT mapped (best-effort, like
    // max_processes below). The only candidate rlimit is RLIMIT_AS, which caps
    // total VIRTUAL address space, not RSS — and on Android/Bionic a fresh
    // execve of mksh/toybox plus the Bionic loader and the Scudo allocator
    // reserve large VA regions (allocator arenas, ~1 MB-per-thread stacks,
    // mmap'd .so segments) far exceeding actual memory use, so a sane-looking
    // cap (a few hundred MB) makes mmap/brk fail with ENOMEM and kills the shell
    // at startup instead of enforcing a useful ceiling. RLIMIT_DATA is likewise
    // ineffective (Scudo uses mmap, not brk), and RLIMIT_RSS is a no-op on
    // modern Linux. There is no interpreter-safe memory rlimit on Android, so we
    // leave memory unbounded here rather than ship a footgun that silently
    // breaks every shell invocation when a policy sets max_memory_mb.
    if let Some(nofile) = policy.limits.max_open_files {
        rlimits.push(Rlimit {
            resource: RlimitResource::NoFile,
            soft: u64::from(nofile),
            hard: u64::from(nofile),
        });
    }
    // limits.max_processes is intentionally NOT mapped: RLIMIT_NPROC is
    // UID-scoped on Android and would count every process of the whole app.
    rlimits.push(Rlimit {
        resource: RlimitResource::Core,
        soft: 0,
        hard: 0,
    });

    // DenyNet plans carry a stable, arch-INDEPENDENT seccomp policy identity
    // recorded at prepare() time. prepare() is host-buildable and arch-agnostic
    // — it runs before the runner knows the device arch, so it cannot compute
    // the per-arch BPF hash (which depends on the concrete socket syscall
    // numbers + AUDIT_ARCH). Instead we hash the POLICY DEFINITION (version name
    // + sorted socket syscall NAME list), which is the same on every device. The
    // runner builds the concrete per-arch BPF from this same policy at run time.
    let seccomp_policy = match network {
        NetProfile::DenyNet => Some(SeccompRef {
            name: platform_android_minijail::net_deny_policy_name().to_string(),
            hash: platform_android_minijail::net_deny_policy_identity_hash(),
        }),
        NetProfile::AllowNet => None,
    };

    Ok(AndroidSandboxPlan {
        target,
        argv: Vec::new(),
        env,
        network,
        rlimits,
        seccomp_policy,
        cleanup: ProcessCleanup::KillProcessGroup,
    })
}

/// Exact-match credential keys that are never forwarded from the caller overlay.
/// The real credential guarantee is the scrub-and-rebuild approach: the
/// inherited process environment is DISCARDED entirely; these entries are only
/// defense-in-depth for explicit caller overlays and are NOT a complete
/// credential list.
const ENV_DENYLIST_EXACT: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// Returns `true` if `key` is denied by the security policy.
///
/// Prefix rules (`LD_`, `DYLD_`) catch dynamic-linker injection vectors;
/// exact matches cover credential keys.
fn is_denied(key: &str) -> bool {
    key.starts_with("LD_") || key.starts_with("DYLD_") || ENV_DENYLIST_EXACT.contains(&key)
}

/// Build the child environment: the inherited environment is DISCARDED
/// entirely and rebuilt from the spec's allowlist table (spec r3 §Environment);
/// `caller_env` (the tool's explicit `cmd.env`) overlays last — explicit wins,
/// except keys rejected by the security policy (see `is_denied`).
///
/// The real credential guarantee is the scrub-and-rebuild (inherited env never
/// enters); the denylist is only defense-in-depth for explicit caller overlays
/// and is not a complete credential list.
#[must_use]
pub fn build_shell_env<S: std::hash::BuildHasher>(
    workspace_root: &Path,
    cache_dir: &Path,
    bundled_helper_dir: Option<&Path>,
    caller_env: &HashMap<String, String, S>,
) -> Vec<(String, String)> {
    let path_value = match bundled_helper_dir {
        Some(dir) => format!("{}:/system/bin", dir.display()),
        None => "/system/bin".to_string(),
    };
    // P4: add GIT_CONFIG_NOSYSTEM=1 when the bundled git ships (spec §Environment).
    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), workspace_root.display().to_string()),
        ("TMPDIR".into(), cache_dir.display().to_string()),
        ("PATH".into(), path_value),
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
        ("ANDROID_ROOT".into(), "/system".into()),
        ("ANDROID_DATA".into(), "/data".into()),
    ];
    // Collect overlay entries and sort by key for deterministic ordering
    // (receipts may hash env in P2; HashMap iteration order is not stable).
    let mut overlay: Vec<(&String, &String)> = caller_env
        .iter()
        .filter(|(k, _)| {
            // Skip malformed keys: execve splits at the FIRST '=', so a key
            // like "LD_PRELOAD=x" would assemble into "LD_PRELOAD=x=y",
            // bypassing the exact-match denylist and setting LD_PRELOAD anyway.
            // NUL bytes and empty keys are also invalid in environ.
            !k.is_empty() && !k.contains('=') && !k.contains('\0') && !is_denied(k)
        })
        .collect();
    overlay.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (k, v) in overlay {
        if let Some(slot) = env.iter_mut().find(|(name, _)| name == k) {
            slot.1.clone_from(v);
        } else {
            env.push((k.clone(), v.clone()));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::{NetworkPolicy, ResourceLimits};
    use platform_api::{SandboxError, SandboxPolicy};
    use std::collections::HashMap;

    #[test]
    fn plan_types_construct() {
        let plan = AndroidSandboxPlan {
            target: ExecTarget::SystemShell,
            argv: vec!["sh".into(), "-c".into(), "true".into()],
            env: vec![("HOME".into(), "/data/x".into())],
            network: NetProfile::DenyNet,
            rlimits: vec![Rlimit {
                resource: RlimitResource::Core,
                soft: 0,
                hard: 0,
            }],
            seccomp_policy: None,
            cleanup: ProcessCleanup::KillProcessGroup,
        };
        assert!(matches!(plan.target, ExecTarget::SystemShell));
        assert!(matches!(plan.network, NetProfile::DenyNet));
    }

    fn base_policy() -> SandboxPolicy {
        SandboxPolicy {
            network: NetworkPolicy::Disabled,
            writable_paths: vec![],
            denied_paths: vec![],
            allow_subprocess: true,
            limits: ResourceLimits::default(),
        }
    }

    fn assert_unavailable_naming(result: Result<AndroidSandboxPlan, SandboxError>, needle: &str) {
        match result {
            Err(SandboxError::Unavailable(msg)) => {
                assert!(msg.contains(needle), "message {msg:?} must name {needle:?}");
            }
            other => panic!("expected Unavailable naming {needle:?}, got {other:?}"),
        }
    }

    #[test]
    fn loopback_only_fails_closed_with_named_guarantee() {
        let mut p = base_policy();
        p.network = NetworkPolicy::LoopbackOnly;
        assert_unavailable_naming(
            plan_from_policy(ExecTarget::SystemShell, &p, vec![]),
            "loopback-only",
        );
    }

    #[test]
    fn fs_confinement_fails_closed() {
        let mut p = base_policy();
        p.writable_paths = vec![PathBuf::from("/data/x")];
        assert_unavailable_naming(
            plan_from_policy(ExecTarget::SystemShell, &p, vec![]),
            "Landlock",
        );
        let mut p = base_policy();
        p.denied_paths = vec![PathBuf::from("/data/y")];
        assert_unavailable_naming(
            plan_from_policy(ExecTarget::SystemShell, &p, vec![]),
            "Landlock",
        );
    }

    #[test]
    fn subprocess_denial_fails_closed() {
        let mut p = base_policy();
        p.allow_subprocess = false;
        assert_unavailable_naming(
            plan_from_policy(ExecTarget::SystemShell, &p, vec![]),
            "subprocess",
        );
    }

    #[test]
    fn networked_system_shell_fails_closed() {
        let mut p = base_policy();
        p.network = NetworkPolicy::Allowed;
        assert_unavailable_naming(
            plan_from_policy(ExecTarget::SystemShell, &p, vec![]),
            "networked shell",
        );
    }

    #[test]
    fn networked_bundled_helper_is_allowed() {
        let mut p = base_policy();
        p.network = NetworkPolicy::Allowed;
        let plan = plan_from_policy(
            ExecTarget::BundledHelper {
                name: "git".into(),
                path: PathBuf::from("/data/app/x/lib/arm64/libgit.so"),
                hash: "abc".into(),
            },
            &p,
            vec![],
        )
        .expect("bundled helper may request network");
        assert_eq!(plan.network, NetProfile::AllowNet);
    }

    #[test]
    fn limits_map_to_rlimits_with_core_always_zero() {
        let mut p = base_policy();
        p.limits = ResourceLimits {
            max_cpu_seconds: Some(30),
            max_memory_mb: Some(512), // intentionally NOT mapped (no interpreter-safe rlimit)
            max_processes: Some(8),   // intentionally NOT mapped (UID-scoped NPROC)
            max_open_files: Some(256),
        };
        let plan = plan_from_policy(ExecTarget::SystemShell, &p, vec![]).expect("plan");
        assert!(plan.rlimits.contains(&Rlimit {
            resource: RlimitResource::Cpu,
            soft: 30,
            hard: 30
        }));
        // RLIMIT_AS must NOT be mapped: it caps virtual address space (not RSS)
        // and would ENOMEM-kill mksh/toybox + Bionic/Scudo at startup.
        assert!(
            !plan
                .rlimits
                .iter()
                .any(|r| r.resource == RlimitResource::As),
            "max_memory_mb must not map to RLIMIT_AS (breaks interpreters on Android)"
        );
        assert!(plan.rlimits.contains(&Rlimit {
            resource: RlimitResource::NoFile,
            soft: 256,
            hard: 256
        }));
        assert!(plan.rlimits.contains(&Rlimit {
            resource: RlimitResource::Core,
            soft: 0,
            hard: 0
        }));
        // max_memory_mb and max_processes intentionally unmapped:
        assert_eq!(
            plan.rlimits.len(),
            3,
            "neither RLIMIT_AS (memory) nor NPROC is mapped in v1"
        );
        assert_eq!(plan.network, NetProfile::DenyNet);
        // P2: DenyNet plans carry the net-deny policy identity (arch-independent).
        let sref = plan
            .seccomp_policy
            .as_ref()
            .expect("DenyNet plan records a SeccompRef");
        assert_eq!(sref.name, "net-deny-v1");
        assert_eq!(
            sref.hash,
            platform_android_minijail::net_deny_policy_identity_hash(),
            "hash is the arch-independent policy identity"
        );
        assert_eq!(plan.cleanup, ProcessCleanup::KillProcessGroup);
    }

    #[test]
    fn allow_net_plan_has_no_seccomp_ref() {
        let mut p = base_policy();
        p.network = NetworkPolicy::Allowed;
        let plan = plan_from_policy(
            ExecTarget::BundledHelper {
                name: "git".into(),
                path: PathBuf::from("/data/app/x/lib/arm64/libgit.so"),
                hash: "abc".into(),
            },
            &p,
            vec![],
        )
        .expect("allow-net bundled helper plan");
        assert_eq!(plan.network, NetProfile::AllowNet);
        assert!(
            plan.seccomp_policy.is_none(),
            "AllowNet installs no net-deny filter, so no SeccompRef"
        );
    }

    #[test]
    fn env_is_scrubbed_and_rebuilt_from_allowlist() {
        let mut inherited = HashMap::new();
        inherited.insert("LD_PRELOAD".to_string(), "/evil.so".to_string());
        inherited.insert("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string());
        let env = build_shell_env(
            std::path::Path::new("/data/user/0/app/files/workspace"),
            std::path::Path::new("/data/user/0/app/cache"),
            None,
            &inherited,
        );
        let map: HashMap<_, _> = env.iter().cloned().collect();
        assert_eq!(
            map.get("HOME").map(String::as_str),
            Some("/data/user/0/app/files/workspace")
        );
        assert_eq!(
            map.get("TMPDIR").map(String::as_str),
            Some("/data/user/0/app/cache")
        );
        assert_eq!(map.get("PATH").map(String::as_str), Some("/system/bin"));
        assert_eq!(map.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert_eq!(map.get("TERM").map(String::as_str), Some("dumb"));
        assert_eq!(map.get("ANDROID_ROOT").map(String::as_str), Some("/system"));
        assert_eq!(map.get("ANDROID_DATA").map(String::as_str), Some("/data"));
        assert!(
            !map.contains_key("LD_PRELOAD"),
            "inherited env must be scrubbed"
        );
        assert!(!map.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn bundled_dir_shadows_system_in_path_and_caller_env_overlays() {
        let mut caller = HashMap::new();
        caller.insert("GIT_TRACE".to_string(), "1".to_string());
        caller.insert("TERM".to_string(), "xterm".to_string()); // explicit wins
        let env = build_shell_env(
            std::path::Path::new("/w"),
            std::path::Path::new("/c"),
            Some(std::path::Path::new("/data/app/x/lib/arm64")),
            &caller,
        );
        let map: HashMap<_, _> = env.iter().cloned().collect();
        assert_eq!(
            map.get("PATH").map(String::as_str),
            Some("/data/app/x/lib/arm64:/system/bin")
        );
        assert_eq!(map.get("GIT_TRACE").map(String::as_str), Some("1"));
        assert_eq!(map.get("TERM").map(String::as_str), Some("xterm"));
    }

    #[test]
    fn disabled_network_with_bundled_helper_maps_to_deny_net() {
        let p = base_policy(); // network: Disabled
        let plan = plan_from_policy(
            ExecTarget::BundledHelper {
                name: "git".into(),
                path: PathBuf::from("/data/app/x/lib/arm64/libgit.so"),
                hash: "abc".into(),
            },
            &p,
            vec![],
        )
        .expect("deny-net bundled helper plan");
        assert_eq!(plan.network, NetProfile::DenyNet);
    }

    #[test]
    fn malformed_overlay_keys_are_skipped() {
        let mut caller = HashMap::new();
        caller.insert("LD_PRELOAD=x".to_string(), "y".to_string());
        caller.insert(String::new(), "empty".to_string());
        caller.insert("OK_KEY".to_string(), "ok".to_string());
        let env = build_shell_env(
            std::path::Path::new("/w"),
            std::path::Path::new("/c"),
            None,
            &caller,
        );
        assert!(env.iter().all(|(k, _)| !k.contains('=') && !k.is_empty()));
        let map: HashMap<_, _> = env.iter().cloned().collect();
        assert_eq!(map.get("OK_KEY").map(String::as_str), Some("ok"));
    }

    #[test]
    fn denylist_is_prefix_based_for_loader_vars_and_case_sensitive() {
        let mut caller = HashMap::new();
        caller.insert("LD_AUDIT".to_string(), "x".to_string()); // prefix-denied
        caller.insert("DYLD_INSERT_LIBRARIES".to_string(), "x".to_string()); // prefix-denied
        caller.insert("ld_preload".to_string(), "inert-on-linux".to_string()); // lowercase passes (Bionic ignores it)
        let env = build_shell_env(
            std::path::Path::new("/w"),
            std::path::Path::new("/c"),
            None,
            &caller,
        );
        let map: HashMap<_, _> = env.iter().cloned().collect();
        assert!(!map.contains_key("LD_AUDIT"));
        assert!(!map.contains_key("DYLD_INSERT_LIBRARIES"));
        assert_eq!(
            map.get("ld_preload").map(String::as_str),
            Some("inert-on-linux")
        );
    }
}
