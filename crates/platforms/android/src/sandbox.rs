//! [`lingxi_core::host::Sandbox`] impl for Android — validation + plan construction only;
//! never spawns (spec r3 §`AndroidMinijailSandbox`).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::host::{
    BackendPlanHandle, ProcessCommand, Sandbox, SandboxBackend, SandboxCapability, SandboxError,
    SandboxPolicy, SandboxedCommand, SandboxedTag,
};

use crate::capabilities::CapabilityCache;
use crate::config::AndroidShellConfig;
use crate::policy::{build_shell_env, plan_from_policy, ExecTarget};

/// Android sandbox: maps policies onto Minijail plans, fail-closed.
pub struct AndroidMinijailSandbox {
    cfg: AndroidShellConfig,
    caps: Arc<CapabilityCache>,
}

impl AndroidMinijailSandbox {
    /// Construct from host config + the (eagerly populated) capability cache.
    #[must_use]
    pub fn new(cfg: AndroidShellConfig, caps: Arc<CapabilityCache>) -> Self {
        Self { cfg, caps }
    }

    /// Canonicalize `cwd` (default: workspace root) and require containment
    /// inside the canonicalized workspace root — symlink escapes are refused.
    ///
    /// Returns `(canonical_root, canonical_cwd)`. The root is used as
    /// `workspace_root` for `build_shell_env` (so HOME is always the workspace
    /// root, not the per-command cwd); the cwd is set on `inner.cwd`.
    ///
    /// Input hygiene, not a boundary (spec non-goal 2): the directory can be
    /// swapped after this check (canonicalize-then-use); the app UID is the
    /// actual boundary.
    fn resolve_cwd(&self, cwd: Option<&PathBuf>) -> Result<(PathBuf, PathBuf), SandboxError> {
        let root = self
            .cfg
            .shell_workspace_root
            .canonicalize()
            .map_err(|e| SandboxError::PathCanonicalize(format!("workspace root: {e}")))?;
        let requested = cwd.cloned().unwrap_or_else(|| root.clone());
        let canon = requested
            .canonicalize()
            .map_err(|e| SandboxError::PathCanonicalize(format!("{}: {e}", requested.display())))?;
        if !canon.starts_with(&root) {
            return Err(SandboxError::SymlinkEscape(canon.display().to_string()));
        }
        Ok((root, canon))
    }

    /// Enforce W^X on a bundled-helper exec target, fail-closed.
    ///
    /// Android only permits `execve` of files under `nativeLibraryDir` (the
    /// installer-extracted, app-read-only native-lib root); `filesDir`/`cacheDir`
    /// and friends are no-exec / app-writable. This makes the exec target's
    /// LOCATION a security property — and the content-hash identity check in the
    /// runner pins bytes, NOT location, so it cannot substitute for it.
    ///
    /// Until now the invariant was only an implicit convention (the android-aar
    /// bootstrap derives the path from `nativeLibraryDir`). This turns it into an
    /// enforced check so a refactor / alternate caller that puts
    /// `bundled_mksh_path` under a writable root cannot reach `execve`: the path
    /// must canonicalize under `native_library_dir` and must NOT fall under any
    /// `app_writable_roots` entry. Gives the previously-dead `app_writable_roots`
    /// field a real consumer.
    fn validate_bundled_exec_path(&self, path: &std::path::Path) -> Result<(), SandboxError> {
        let nativelib = self.cfg.native_library_dir.canonicalize().map_err(|e| {
            SandboxError::PathCanonicalize(format!(
                "native_library_dir {}: {e}",
                self.cfg.native_library_dir.display()
            ))
        })?;
        let canon = path.canonicalize().map_err(|e| {
            SandboxError::PathCanonicalize(format!("bundled exec target {}: {e}", path.display()))
        })?;
        if !canon.starts_with(&nativelib) {
            return Err(SandboxError::Unavailable(format!(
                "bundled exec target {} is not under nativeLibraryDir {} (W^X)",
                canon.display(),
                nativelib.display()
            )));
        }
        for root in &self.cfg.app_writable_roots {
            // A non-canonicalizable writable root cannot contain the (already
            // canonicalized) target, so skip it rather than fail the whole run.
            if let Ok(wr) = root.canonicalize() {
                if canon.starts_with(&wr) {
                    return Err(SandboxError::Unavailable(format!(
                        "bundled exec target {} is under app-writable root {} (W^X violation)",
                        canon.display(),
                        wr.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Sandbox for AndroidMinijailSandbox {
    fn is_available(&self) -> bool {
        self.caps.get().available()
    }

    fn backend(&self) -> SandboxBackend {
        SandboxBackend::AndroidMinijail
    }

    /// Build a sandboxed command for the `SystemShell` target.
    ///
    /// For the `SystemShell` target the caller's `command` field is ignored and
    /// normalized to `/system/bin/sh`; only `args` are honored.
    fn prepare(
        &self,
        cmd: ProcessCommand,
        policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError> {
        let caps = self.caps.get();
        if !caps.available() {
            return Err(SandboxError::Unavailable(
                caps.reason
                    .unwrap_or_else(|| "android sandbox unavailable".into()),
            ));
        }

        // P2 NOTE: when the net-deny seccomp filter lands, DenyNet plan
        // admission must additionally gate on caps.seccomp_filter +
        // caps.net_deny_verified — available() alone is the coarse
        // registration gate, not the per-plan requirement check.

        let (canonical_root, canonical_cwd) = self.resolve_cwd(cmd.cwd.as_ref())?;

        // P5b: when the host bundled mksh+toybox (path + hash + applet dir all
        // present), exec the bundled mksh and lead PATH with the applet dir so
        // toybox shadows /system/bin. Otherwise fall back to /system/bin/sh.
        // Shell is always DenyNet (spec D10) regardless of target.
        let (target, bundled_helper_dir) = if self.cfg.bundled_ready() {
            let path = self
                .cfg
                .bundled_mksh_path
                .clone()
                .expect("bundled_ready() guarantees mksh path");
            let hash = self
                .cfg
                .bundled_mksh_hash
                .clone()
                .expect("bundled_ready() guarantees mksh hash");
            // W^X: the bundled exec target must live under nativeLibraryDir and
            // outside every app-writable root, or we fall closed (never execve a
            // writable/illegal path — the hash check pins bytes, not location).
            self.validate_bundled_exec_path(&path)?;
            (
                ExecTarget::BundledHelper {
                    name: "mksh".into(),
                    path,
                    hash,
                },
                self.cfg.bundled_applet_dir.as_deref(),
            )
        } else {
            (ExecTarget::SystemShell, None)
        };

        // HOME must be the workspace root, not the per-command cwd (spec env
        // table: "HOME = configured shell workspace root").
        let env = build_shell_env(
            &canonical_root,
            &self.cfg.app_cache_root,
            bundled_helper_dir,
            &cmd.env,
        );

        let mut plan = plan_from_policy(target, policy, env.clone())?;
        // argv[0] convention: "sh"; the runner execs /system/bin/sh.
        plan.argv = std::iter::once("sh".to_string())
            .chain(cmd.args.iter().cloned())
            .collect();

        // inner must never contradict plan.env ("the ONLY env the child sees"):
        // normalize command to the actual exec target and replace env with the
        // scrubbed env so that anything auditing/logging inner() sees the truth.
        let inner = ProcessCommand {
            command: "/system/bin/sh".to_string(),
            env: env.into_iter().collect(),
            cwd: Some(canonical_cwd),
            args: cmd.args,
            timeout: cmd.timeout,
            stdin: cmd.stdin,
        };
        Ok(SandboxedCommand::__new_sandboxed_with_plan(
            inner,
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidMinijail,
            },
            BackendPlanHandle::new(plan),
        ))
    }

    fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
        // Trait contract: always mints. The Android RUNNER rejects this tag
        // (spec security invariant #1) — the audit trail still records intent.
        SandboxedCommand::__new_sandboxed(
            cmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.to_string(),
            },
        )
    }

    async fn probe_capability(&self) -> SandboxCapability {
        self.caps.get().to_sandbox_capability()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::AndroidSandboxCapabilities;
    use crate::policy::{AndroidSandboxPlan, NetProfile}; // not in the impl's imports — tests need it explicitly
    use lingxi_core::host::{
        NetworkPolicy, ProcessCommand, ResourceLimits, Sandbox, SandboxPolicy,
    };
    use std::collections::HashMap;

    fn ready_caps() -> AndroidSandboxCapabilities {
        AndroidSandboxCapabilities {
            probed: true,
            minijail_smoke: true,
            no_new_privs: true,
            ..AndroidSandboxCapabilities::default()
        }
    }

    fn shell_cfg(ws: &std::path::Path) -> AndroidShellConfig {
        AndroidShellConfig {
            native_library_dir: ws.join("native-lib"),
            shell_workspace_root: ws.to_path_buf(),
            app_cache_root: ws.join("cache"),
            package_name: "com.example".into(),
            package_version_code: 1,
            app_writable_roots: vec![ws.to_path_buf()],
            enable_shell: true,
            secrets_in_keystore: true,
            shell_data_exposure_accepted: false,
            bundled_mksh_path: None,
            bundled_mksh_hash: None,
            bundled_applet_dir: None,
        }
    }

    fn sandbox_with(
        ws: &std::path::Path,
        caps: AndroidSandboxCapabilities,
    ) -> AndroidMinijailSandbox {
        let cache = std::sync::Arc::new(CapabilityCache::new());
        cache.set(caps);
        AndroidMinijailSandbox::new(shell_cfg(ws), cache)
    }

    fn sandbox_with_cfg(
        cfg: AndroidShellConfig,
        caps: AndroidSandboxCapabilities,
    ) -> AndroidMinijailSandbox {
        let cache = std::sync::Arc::new(CapabilityCache::new());
        cache.set(caps);
        AndroidMinijailSandbox::new(cfg, cache)
    }

    /// `shell_cfg(ws)` with the three bundled fields populated → `bundled_ready()`.
    /// Creates a REALISTIC W^X layout under `ws`: a read-only `native-lib/`
    /// (disjoint from the app-writable roots) holding a real `libmksh.so`, so the
    /// W^X exec-path containment check in `prepare()` passes.
    fn config_with_bundled(
        ws: &std::path::Path,
        _mksh_path: &str,
        applet_dir: &str,
        hash: &str,
    ) -> AndroidShellConfig {
        let nativelib = ws.join("native-lib");
        std::fs::create_dir_all(&nativelib).expect("native-lib dir");
        let mksh = nativelib.join("libmksh.so");
        std::fs::write(&mksh, b"#!stub mksh\n").expect("libmksh.so");
        let writable = ws.join("files");
        std::fs::create_dir_all(&writable).expect("writable dir");
        let mut cfg = shell_cfg(ws);
        cfg.native_library_dir = nativelib;
        // app-writable roots are DISJOINT from native-lib (as on a real device:
        // filesDir/cacheDir vs the installer-extracted lib dir).
        cfg.app_writable_roots = vec![writable];
        cfg.bundled_mksh_path = Some(mksh);
        cfg.bundled_applet_dir = Some(std::path::PathBuf::from(applet_dir));
        cfg.bundled_mksh_hash = Some(hash.to_string());
        cfg
    }

    fn cmd(cwd: Option<std::path::PathBuf>) -> ProcessCommand {
        ProcessCommand {
            command: "/system/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd,
            env: HashMap::new(),
            timeout: None,
            stdin: None,
        }
    }

    fn deny_net_policy() -> SandboxPolicy {
        SandboxPolicy {
            network: NetworkPolicy::Disabled,
            writable_paths: vec![],
            denied_paths: vec![],
            allow_subprocess: true,
            limits: ResourceLimits::default(),
        }
    }

    #[test]
    fn prepare_attaches_android_plan_and_tag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let sc = sb.prepare(cmd(None), &deny_net_policy()).expect("prepare");
        assert!(matches!(
            sc.tag(),
            lingxi_core::host::SandboxedTag::Wrapped {
                backend: lingxi_core::host::SandboxBackend::AndroidMinijail
            }
        ));
        let plan = sc
            .backend_plan()
            .expect("plan attached")
            .downcast::<AndroidSandboxPlan>()
            .expect("android plan type");
        assert!(matches!(plan.target, ExecTarget::SystemShell));
        assert_eq!(plan.network, NetProfile::DenyNet);
        assert_eq!(
            plan.argv,
            vec!["sh".to_string(), "-c".to_string(), "true".to_string()]
        );
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        assert_eq!(
            env.get("HOME").map(String::as_str),
            Some(tmp.path().canonicalize().unwrap().to_str().unwrap())
        );
    }

    #[test]
    fn prepare_targets_bundled_mksh_when_ready() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = config_with_bundled(tmp.path(), "/nl/libmksh.so", "/app/applet-bin", "deadbeef");
        let expected_path = cfg.bundled_mksh_path.clone().unwrap();
        let sb = sandbox_with_cfg(cfg, ready_caps());
        let sc = sb.prepare(cmd(None), &deny_net_policy()).expect("prepare");
        let plan = sc
            .backend_plan()
            .unwrap()
            .downcast::<AndroidSandboxPlan>()
            .unwrap();
        match &plan.target {
            ExecTarget::BundledHelper { name, path, hash } => {
                assert_eq!(name, "mksh");
                assert_eq!(path, &expected_path);
                assert_eq!(hash, "deadbeef");
            }
            other @ ExecTarget::SystemShell => panic!("expected BundledHelper, got {other:?}"),
        }
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        assert!(
            env["PATH"].starts_with("/app/applet-bin:"),
            "applet dir leads PATH"
        );
    }

    #[test]
    fn prepare_rejects_bundled_exec_outside_native_lib() {
        // W^X: a bundled exec target that exists but lives OUTSIDE nativeLibraryDir
        // (e.g. under an app-writable root) must fail closed, never reach execve.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg =
            config_with_bundled(tmp.path(), "/nl/libmksh.so", "/app/applet-bin", "deadbeef");
        // Point the exec target at a real file under the app-writable root.
        let writable = tmp.path().join("files");
        let rogue = writable.join("evil-mksh.so");
        std::fs::write(&rogue, b"#!rogue\n").expect("rogue file");
        cfg.bundled_mksh_path = Some(rogue);
        let sb = sandbox_with_cfg(cfg, ready_caps());
        let err = sb
            .prepare(cmd(None), &deny_net_policy())
            .expect_err("must reject exec target outside nativeLibraryDir");
        assert!(
            matches!(err, SandboxError::Unavailable(ref m) if m.contains("W^X") || m.to_lowercase().contains("nativelibrary")),
            "expected a W^X fail-closed error, got {err:?}"
        );
    }

    #[test]
    fn prepare_targets_system_shell_when_not_bundled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let sc = sb.prepare(cmd(None), &deny_net_policy()).expect("prepare");
        let plan = sc
            .backend_plan()
            .unwrap()
            .downcast::<AndroidSandboxPlan>()
            .unwrap();
        assert!(matches!(plan.target, ExecTarget::SystemShell));
    }

    #[test]
    fn prepare_fails_closed_when_caps_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(
            tmp.path(),
            AndroidSandboxCapabilities::unavailable("no device probe"),
        );
        let err = sb.prepare(cmd(None), &deny_net_policy()).unwrap_err();
        assert!(matches!(
            err,
            lingxi_core::host::SandboxError::Unavailable(_)
        ));
    }

    #[test]
    fn prepare_rejects_cwd_outside_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let err = sb
            .prepare(cmd(Some(outside.path().to_path_buf())), &deny_net_policy())
            .unwrap_err();
        assert!(matches!(
            err,
            lingxi_core::host::SandboxError::SymlinkEscape(_)
        ));
    }

    #[test]
    fn prepare_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let link = tmp.path().join("sneaky");
        std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let err = sb.prepare(cmd(Some(link)), &deny_net_policy()).unwrap_err();
        assert!(matches!(
            err,
            lingxi_core::host::SandboxError::SymlinkEscape(_)
        ));
    }

    #[test]
    fn prepare_propagates_policy_fail_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let mut policy = deny_net_policy();
        policy.network = NetworkPolicy::Allowed; // SystemShell + Allowed = refused
        assert!(sb.prepare(cmd(None), &policy).is_err());
    }

    #[test]
    fn bypass_with_audit_still_mints_bypass_tag() {
        // Trait contract: bypass always returns a SandboxedCommand. The
        // ANDROID RUNNER is what rejects it (Task 10) — not prepare.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let sc = sb.bypass_with_audit(cmd(None), "test-reason");
        assert!(matches!(
            sc.tag(),
            lingxi_core::host::SandboxedTag::BypassAuditedWithReason { .. }
        ));
        assert!(sc.backend_plan().is_none());
    }

    #[tokio::test]
    async fn probe_capability_reads_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let cap = sb.probe_capability().await;
        assert!(cap.available);
        assert!(cap.features.no_new_privileges);
    }

    #[test]
    fn subdir_cwd_keeps_home_at_workspace_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sub = tmp.path().join("repo");
        std::fs::create_dir(&sub).expect("mkdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let sc = sb
            .prepare(cmd(Some(sub.clone())), &deny_net_policy())
            .expect("prepare");
        let plan = sc
            .backend_plan()
            .unwrap()
            .downcast::<AndroidSandboxPlan>()
            .unwrap();
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        let root = tmp.path().canonicalize().unwrap();
        assert_eq!(env.get("HOME").map(String::as_str), root.to_str());
        assert_eq!(
            sc.inner().cwd.as_deref(),
            Some(sub.canonicalize().unwrap().as_path())
        );
    }

    #[test]
    fn nonexistent_cwd_is_path_canonicalize_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let err = sb
            .prepare(cmd(Some(tmp.path().join("missing"))), &deny_net_policy())
            .unwrap_err();
        assert!(matches!(
            err,
            lingxi_core::host::SandboxError::PathCanonicalize(_)
        ));
    }

    #[test]
    fn inner_command_and_env_match_the_plan_not_the_caller() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb = sandbox_with(tmp.path(), ready_caps());
        let mut c = cmd(None);
        c.command = "/system/bin/id".into(); // ignored + normalized
        c.env.insert("LD_PRELOAD".into(), "/evil.so".into()); // scrubbed
        let sc = sb.prepare(c, &deny_net_policy()).expect("prepare");
        assert_eq!(sc.inner().command, "/system/bin/sh");
        assert!(!sc.inner().env.contains_key("LD_PRELOAD"));
        let plan = sc
            .backend_plan()
            .unwrap()
            .downcast::<AndroidSandboxPlan>()
            .unwrap();
        let plan_env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        let inner_env: std::collections::HashMap<_, _> = sc
            .inner()
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(plan_env, inner_env, "inner env must mirror plan env");
    }
}
