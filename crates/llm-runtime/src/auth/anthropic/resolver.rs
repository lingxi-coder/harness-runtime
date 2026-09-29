//! Anthropic auth-source resolver.
//!
//! See spec §30.2 / A6. Inspects environment, stored credentials, and
//! settings to choose exactly one of the documented auth sources. Order is
//! deterministic and intentionally favours managed-context overrides.

#![allow(clippy::struct_excessive_bools)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One of the nine documented Anthropic auth sources (plus [`AuthSource::None`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthSource {
    /// Highest priority: managed contexts force OAuth.
    OAuthClaudeAi,
    /// Environment variable `ANTHROPIC_AUTH_TOKEN` (bearer).
    EnvAuthToken,
    /// Environment variable `ANTHROPIC_API_KEY`.
    EnvApiKey,
    /// File-descriptor inheritance (managed launch).
    FileDescriptor,
    /// API key stored via `SecureStorage`.
    StoredApiKey,
    /// Legacy: `settings.json` `apiKey` field.
    SettingsApiKey,
    /// External script that returns an API key on stdout.
    ApiKeyHelper {
        /// Path to the helper script invoked to fetch the key.
        script_path: PathBuf,
    },
    /// AWS Bedrock credentials in the ambient environment.
    AwsBedrock,
    /// No usable source found.
    None,
}

/// Inputs the resolver inspects to pick an [`AuthSource`].
///
/// Built once during platform init; passed by reference to [`resolve`].
pub struct ResolverContext {
    /// `true` when the HOST launcher forces OAuth even if other sources are
    /// present — claude-code's `KWr()`, see [`host_managed_oauth_only`].
    ///
    /// A managed `forceLoginMethod` policy must NEVER feed this field: the
    /// oracle's credential precedence has no `forceLoginMethod` term at all
    /// (`zb()` @228933355, `e1()` @228936313). That setting only pre-selects
    /// the login method and validates AFTER login (`vst()` @228970823), and a
    /// managed pin combined with an Anthropic-issued credential is REFUSED at
    /// startup (`Gde()` @228967690), never silently re-ranked.
    pub managed_oauth_only: bool,
    /// Captured value of `ANTHROPIC_AUTH_TOKEN`, if set.
    pub env_auth_token: Option<String>,
    /// Captured value of `ANTHROPIC_API_KEY`, if set.
    pub env_api_key: Option<String>,
    /// `true` when the launcher inherited an API key via a file descriptor.
    pub fd_present: bool,
    /// `true` when an OAuth token is present in `SecureStorage`.
    pub has_stored_oauth: bool,
    /// `true` when a raw API key is present in `SecureStorage`.
    pub has_stored_api_key: bool,
    /// API key surfaced by `settings.json` (legacy path).
    pub settings_api_key: Option<String>,
    /// Path to a configured `apiKeyHelper` script, if any.
    pub api_key_helper_script: Option<PathBuf>,
    /// `true` when AWS Bedrock credentials are present.
    pub aws_present: bool,
}

/// `YIt()` (2.1.220 @228930165): `Z.CLAUDE_CODE_REMOTE || jN()`, where `jN()`
/// (@226508771) tests `CLAUDE_CODE_ENTRYPOINT` against the host-launcher set
/// `{claude-desktop, claude-desktop-3p, local-agent}` (@226511006).
fn remote_or_host_entrypoint(remote: Option<&str>, entrypoint: Option<&str>) -> bool {
    // Raw JS truthiness on the env string — the oracle reads `Z.CLAUDE_CODE_REMOTE`
    // directly, NOT through `isEnvTruthy`, so any non-empty value counts.
    remote.is_some_and(|v| !v.is_empty())
        || matches!(
            entrypoint,
            Some("claude-desktop" | "claude-desktop-3p" | "local-agent")
        )
}

/// `KWr()` (2.1.220 @228931361) over explicit env values, so callers (and
/// tests) need not mutate the process environment.
fn host_managed_oauth_only_from(
    remote: Option<&str>,
    entrypoint: Option<&str>,
    host_auth_env_var: Option<&str>,
) -> bool {
    remote_or_host_entrypoint(remote, entrypoint)
        && !host_auth_env_var.is_some_and(|v| !v.is_empty())
        && entrypoint != Some("claude-desktop-3p")
}

/// `KWr()` (2.1.220 @228931361) read from the process environment.
///
/// This is the ONLY predicate that demotes an env `ANTHROPIC_AUTH_TOKEN` /
/// `ANTHROPIC_API_KEY` below the stored Claude.ai session inside `zb()`
/// (@228933355: `a = (n||i) && !KWr() || (r||s) && !YIt()`), i.e. the only
/// thing that legitimately sets [`ResolverContext::managed_oauth_only`].
#[must_use]
pub fn host_managed_oauth_only() -> bool {
    let remote = std::env::var("CLAUDE_CODE_REMOTE").ok();
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT").ok();
    let host_auth_env_var = std::env::var("CLAUDE_CODE_HOST_AUTH_ENV_VAR").ok();
    host_managed_oauth_only_from(
        remote.as_deref(),
        entrypoint.as_deref(),
        host_auth_env_var.as_deref(),
    )
}

/// Resolve the highest-priority auth source from the supplied context.
///
/// Order: managed OAuth → env auth token → env API key → FD → stored OAuth →
/// stored API key → settings → helper script → AWS Bedrock → [`AuthSource::None`].
#[must_use]
pub fn resolve(ctx: &ResolverContext) -> AuthSource {
    if ctx.managed_oauth_only && ctx.has_stored_oauth {
        return AuthSource::OAuthClaudeAi;
    }
    if ctx.env_auth_token.is_some() {
        return AuthSource::EnvAuthToken;
    }
    if ctx.env_api_key.is_some() {
        return AuthSource::EnvApiKey;
    }
    if ctx.fd_present {
        return AuthSource::FileDescriptor;
    }
    if ctx.has_stored_oauth {
        return AuthSource::OAuthClaudeAi;
    }
    if ctx.has_stored_api_key {
        return AuthSource::StoredApiKey;
    }
    if ctx.settings_api_key.is_some() {
        return AuthSource::SettingsApiKey;
    }
    if let Some(p) = &ctx.api_key_helper_script {
        return AuthSource::ApiKeyHelper {
            script_path: p.clone(),
        };
    }
    if ctx.aws_present {
        return AuthSource::AwsBedrock;
    }
    AuthSource::None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host-launcher forcing (`KWr()` true) is what lets the stored session
    /// outrank an env bearer.
    #[test]
    fn managed_oauth_wins() {
        let ctx = ResolverContext {
            managed_oauth_only: true,
            env_auth_token: Some("x".into()),
            env_api_key: None,
            fd_present: false,
            has_stored_oauth: true,
            has_stored_api_key: false,
            settings_api_key: None,
            api_key_helper_script: None,
            aws_present: false,
        };
        assert!(matches!(resolve(&ctx), AuthSource::OAuthClaudeAi));
    }

    /// Without the HOST predicate an env `ANTHROPIC_API_KEY` (or bearer) keeps
    /// outranking the stored session — `zb()`'s `(n||i) && !KWr()` term. A
    /// managed `forceLoginMethod: "claudeai"` policy is NOT `KWr()` and must
    /// never flip this.
    #[test]
    fn env_key_outranks_stored_oauth_without_host_forcing() {
        let ctx = ResolverContext {
            managed_oauth_only: false,
            env_auth_token: None,
            env_api_key: Some("sk-ant".into()),
            fd_present: false,
            has_stored_oauth: true,
            has_stored_api_key: false,
            settings_api_key: None,
            api_key_helper_script: None,
            aws_present: false,
        };
        assert!(matches!(resolve(&ctx), AuthSource::EnvApiKey));
    }

    /// `KWr()` = `YIt() && !CLAUDE_CODE_HOST_AUTH_ENV_VAR && entrypoint !=
    /// "claude-desktop-3p"`, with `YIt()` = `CLAUDE_CODE_REMOTE || entrypoint ∈
    /// {claude-desktop, claude-desktop-3p, local-agent}`.
    #[test]
    fn host_managed_predicate_matches_kwr() {
        // YIt() arms.
        assert!(host_managed_oauth_only_from(Some("1"), None, None));
        assert!(host_managed_oauth_only_from(
            Some("anything"),
            Some("cli"),
            None
        ));
        assert!(host_managed_oauth_only_from(
            None,
            Some("claude-desktop"),
            None
        ));
        assert!(host_managed_oauth_only_from(
            None,
            Some("local-agent"),
            None
        ));
        // Empty env value is falsy in JS.
        assert!(!host_managed_oauth_only_from(Some(""), Some("cli"), None));
        assert!(!host_managed_oauth_only_from(None, None, None));
        assert!(!host_managed_oauth_only_from(None, Some("cli"), None));
        // Both KWr() exclusions.
        assert!(!host_managed_oauth_only_from(
            Some("1"),
            Some("cli"),
            Some("ANTHROPIC_API_KEY")
        ));
        assert!(!host_managed_oauth_only_from(
            None,
            Some("claude-desktop-3p"),
            None
        ));
        // …and the 3p entrypoint stays excluded even under CLAUDE_CODE_REMOTE.
        assert!(!host_managed_oauth_only_from(
            Some("1"),
            Some("claude-desktop-3p"),
            None
        ));
    }

    /// The env-reading wrapper must look at the SAME three variables — a
    /// mis-named var would silently disable host forcing everywhere.
    /// `host_managed_oauth_only` reads the process env, so this test serializes
    /// on a lock and restores the previous values.
    #[test]
    fn host_managed_oauth_only_reads_the_kwr_env_vars() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        struct EnvGuard {
            key: &'static str,
            prev: Option<String>,
        }
        impl EnvGuard {
            fn set(key: &'static str, val: &str) -> Self {
                let prev = std::env::var(key).ok();
                std::env::set_var(key, val);
                Self { key, prev }
            }
            fn unset(key: &'static str) -> Self {
                let prev = std::env::var(key).ok();
                std::env::remove_var(key);
                Self { key, prev }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }

        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _remote = EnvGuard::unset("CLAUDE_CODE_REMOTE");
        let _entry = EnvGuard::unset("CLAUDE_CODE_ENTRYPOINT");
        let _host = EnvGuard::unset("CLAUDE_CODE_HOST_AUTH_ENV_VAR");
        assert!(!host_managed_oauth_only());
        {
            let _e = EnvGuard::set("CLAUDE_CODE_ENTRYPOINT", "claude-desktop");
            assert!(host_managed_oauth_only());
            let _h = EnvGuard::set("CLAUDE_CODE_HOST_AUTH_ENV_VAR", "ANTHROPIC_API_KEY");
            assert!(!host_managed_oauth_only());
        }
        let _r = EnvGuard::set("CLAUDE_CODE_REMOTE", "1");
        assert!(host_managed_oauth_only());
    }

    #[test]
    fn none_when_no_source() {
        let ctx = ResolverContext {
            managed_oauth_only: false,
            env_auth_token: None,
            env_api_key: None,
            fd_present: false,
            has_stored_oauth: false,
            has_stored_api_key: false,
            settings_api_key: None,
            api_key_helper_script: None,
            aws_present: false,
        };
        assert!(matches!(resolve(&ctx), AuthSource::None));
    }
}
