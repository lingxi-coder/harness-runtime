//! `--dangerously-skip-permissions` environment safety guards — port of
//! claude-code `setup.ts:395-443`. Runs only when bypass is requested/resolved.
//!
//! Pure decision logic over an injected [`BypassEnv`] so the matrix is testable
//! without root / Docker / network. The real impl lives in the CLI
//! (`apps/cli/src/bypass_env.rs`): `geteuid`, `/.dockerenv`, env, and a 1s
//! HTTP HEAD probe.

/// Host-environment probes the guard needs. Injected so tests can drive every
/// combination.
#[async_trait::async_trait]
pub trait BypassEnv: Send + Sync {
    /// `process.platform === 'win32'`.
    fn is_windows(&self) -> bool;
    /// The process uid checked against 0. claude-code calls `process.getuid()`
    /// — the REAL uid (`setup.ts:404`), so the production impl returns
    /// `getuid()`, NOT the effective uid (the method name is a slight misnomer
    /// kept for the trait contract). Non-unix hosts return a non-zero sentinel
    /// so check 1 is a no-op.
    fn effective_uid(&self) -> u32;
    /// Read a process env var.
    fn env(&self, key: &str) -> Option<String>;
    /// `getIsDocker()` (`envDynamic.ts:11`): linux && `/.dockerenv` exists.
    fn is_docker(&self) -> bool;
    /// `hasInternetAccess()` (`env.ts:28`): a 1s HEAD to `http://1.1.1.1`.
    async fn has_internet(&self) -> bool;
}

/// `isEnvTruthy` (`envUtils.ts:32-37`): unset/empty ⇒ false; else
/// lowercase-trim ∈ {1, true, yes, on}.
fn env_truthy(v: Option<String>) -> bool {
    v.is_some_and(|s| {
        matches!(
            s.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Enforce the bypass safety preconditions. `Err(message)` means the
/// environment is unsafe — the caller prints it to stderr and exits 1 (TS
/// `console.error` + `process.exit(1)`).
///
/// 2.1.211 (`setup.ts`, `refuseBypassUnderRoot`/`IXl`) enforces ONLY the
/// root/sudo refusal. The old ant-gated "can only be used in Docker/sandbox
/// containers with no internet access but got Docker: …" refusal was removed
/// upstream (0 hits for `but got Docker` / `can only be used in Docker` across
/// the 2.1.211 corpus). It is therefore no longer ported — its presence was
/// anti-parity dead weight that fired (with a 1s internet probe) whenever
/// `USER_TYPE=ant`, which 2.1.211 no longer does. The `is_docker` /
/// `has_internet` trait probes are retained for the injected-env contract but
/// are now unused by the guard.
///
/// # Errors
/// Returns the byte-exact 2.1.211 refusal message for a root/sudo session.
pub async fn enforce_bypass_safety(env: &dyn BypassEnv) -> Result<(), String> {
    // Root/sudo refusal (all builds; 2.1.211 `refuseBypassUnderRoot`/`IXl`).
    if !env.is_windows()
        && env.effective_uid() == 0
        && env.env("IS_SANDBOX").as_deref() != Some("1")
        && !env_truthy(env.env("LINGXI_BUBBLEWRAP"))
    {
        return Err(
            "--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons"
                .to_string(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv {
        windows: bool,
        euid: u32,
        docker: bool,
        internet: bool,
        vars: HashMap<String, String>,
    }
    impl Default for FakeEnv {
        fn default() -> Self {
            Self {
                windows: false,
                euid: 1000,
                docker: false,
                internet: false,
                vars: HashMap::new(),
            }
        }
    }
    #[async_trait::async_trait]
    impl BypassEnv for FakeEnv {
        fn is_windows(&self) -> bool {
            self.windows
        }
        fn effective_uid(&self) -> u32 {
            self.euid
        }
        fn env(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn is_docker(&self) -> bool {
            self.docker
        }
        async fn has_internet(&self) -> bool {
            self.internet
        }
    }

    #[tokio::test]
    async fn non_root_passes() {
        let e = FakeEnv::default();
        assert!(enforce_bypass_safety(&e).await.is_ok());
    }

    #[tokio::test]
    async fn root_without_sandbox_is_refused() {
        let e = FakeEnv {
            euid: 0,
            ..FakeEnv::default()
        };
        let err = enforce_bypass_safety(&e).await.unwrap_err();
        assert_eq!(err, "--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons");
    }

    #[tokio::test]
    async fn root_with_is_sandbox_passes_check_one() {
        let mut e = FakeEnv {
            euid: 0,
            ..FakeEnv::default()
        };
        e.vars.insert("IS_SANDBOX".into(), "1".into());
        assert!(enforce_bypass_safety(&e).await.is_ok());
    }

    #[tokio::test]
    async fn root_with_bubblewrap_passes_check_one() {
        let mut e = FakeEnv {
            euid: 0,
            ..FakeEnv::default()
        };
        e.vars.insert("LINGXI_BUBBLEWRAP".into(), "1".into());
        assert!(enforce_bypass_safety(&e).await.is_ok());
    }

    // BYPASS-ANT-DOCKER-07: 2.1.211 removed the ant-gated Docker/no-internet
    // refusal. `USER_TYPE=ant` on an un-sandboxed, internet-connected host must
    // now PASS the guard (only the root/sudo refusal survives), and the old
    // "can only be used in Docker/sandbox" message must never be emitted.

    #[tokio::test]
    async fn ant_not_sandboxed_passes_after_docker_refusal_removed() {
        let mut e = FakeEnv::default();
        e.vars.insert("USER_TYPE".into(), "ant".into());
        assert!(enforce_bypass_safety(&e).await.is_ok());
    }

    #[tokio::test]
    async fn ant_sandboxed_with_internet_passes_after_docker_refusal_removed() {
        let mut e = FakeEnv {
            docker: true,
            internet: true,
            ..FakeEnv::default()
        };
        e.vars.insert("USER_TYPE".into(), "ant".into());
        assert!(enforce_bypass_safety(&e).await.is_ok());
    }

    #[tokio::test]
    async fn ant_root_still_refused_by_root_check() {
        // The surviving root/sudo refusal still fires for an ant root session.
        let mut e = FakeEnv {
            euid: 0,
            ..FakeEnv::default()
        };
        e.vars.insert("USER_TYPE".into(), "ant".into());
        let err = enforce_bypass_safety(&e).await.unwrap_err();
        assert_eq!(err, "--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons");
        assert!(!err.contains("Docker"));
    }

    #[tokio::test]
    async fn non_ant_unsandboxed_passes() {
        let e = FakeEnv::default(); // not sandboxed, non-ant → passes
        assert!(enforce_bypass_safety(&e).await.is_ok());
    }
}
