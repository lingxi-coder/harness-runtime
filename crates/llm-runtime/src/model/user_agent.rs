//! Byte-locked `claude-cli` User-Agent builder.
//!
//! Ports `getUserAgent()` from `claude-code/src/utils/http.ts:18-35`.
//!
//! Template (exact bytes — do NOT alter the `claude-cli/` prefix or separator
//! strings without updating the log-filtering rules):
//!
//! ```text
//! claude-cli/<VERSION> (<USER_TYPE>, <ENTRYPOINT>[, agent-sdk/<V>][, client-app/<APP>][, workload/<W>])
//! ```
//!
//! - `<USER_TYPE>`: `USER_TYPE` env **raw**, literal `"undefined"` when unset
//!   (JS semantics: `process.env.USER_TYPE` evaluates to `undefined` when
//!   absent, and template-literal interpolation stringifies that as
//!   `"undefined"`).
//! - `<ENTRYPOINT>`: `CLAUDE_CODE_ENTRYPOINT` env, default `"cli"`.
//! - Optional suffix `", agent-sdk/<V>"` when `CLAUDE_AGENT_SDK_VERSION` is
//!   set.
//! - Optional suffix `", client-app/<APP>"` when `CLAUDE_AGENT_SDK_CLIENT_APP`
//!   is set.
//! - Optional suffix `", workload/<W>"` when a workload tag is supplied by the
//!   caller. `getWorkload()` in the TS source resolves this from
//!   `AsyncLocalStorage` (not an env var); callers populate
//!   [`UserAgentEnv::workload`] from their async context.

#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Injectable environment snapshot
// ---------------------------------------------------------------------------

/// Snapshot of the environment variables (and call-scoped state) that feed the
/// `claude-cli` User-Agent header.
///
/// Use [`UserAgentEnv::from_process_env`] for production callers.
/// Construct directly (or via `Default`) for deterministic unit tests.
#[derive(Debug, Default, Clone)]
pub struct UserAgentEnv {
    /// Value of `USER_TYPE` env.  `None` → serialised as `"undefined"` in the
    /// header (JS semantics).
    pub user_type: Option<String>,

    /// Value of `CLAUDE_CODE_ENTRYPOINT` env.  `None` → defaults to `"cli"`.
    pub entrypoint: Option<String>,

    /// Value of `CLAUDE_AGENT_SDK_VERSION` env.  `None` → suffix omitted.
    pub agent_sdk_version: Option<String>,

    /// Value of `CLAUDE_AGENT_SDK_CLIENT_APP` env.  `None` → suffix omitted.
    pub client_app: Option<String>,

    /// Turn-/process-scoped workload tag (populated from `getWorkload()` /
    /// `AsyncLocalStorage` in JS; passed in by the Rust caller from its async
    /// context).  `None` → suffix omitted.
    pub workload: Option<String>,
}

impl UserAgentEnv {
    /// Build a [`UserAgentEnv`] by reading the real process environment.
    ///
    /// Mirrors the env reads in `http.ts:19-33`:
    /// - `USER_TYPE`
    /// - `CLAUDE_CODE_ENTRYPOINT`
    /// - `CLAUDE_AGENT_SDK_VERSION`
    /// - `CLAUDE_AGENT_SDK_CLIENT_APP`
    ///
    /// `workload` is initialised to `None`; the caller must set it from its
    /// async context if workload tagging is needed.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            user_type: std::env::var("USER_TYPE").ok(),
            entrypoint: std::env::var("CLAUDE_CODE_ENTRYPOINT").ok(),
            agent_sdk_version: std::env::var("CLAUDE_AGENT_SDK_VERSION").ok(),
            client_app: std::env::var("CLAUDE_AGENT_SDK_CLIENT_APP").ok(),
            workload: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Build the `claude-cli` User-Agent header string.
///
/// Byte-locked to `getUserAgent()` in `claude-code/src/utils/http.ts:34`:
///
/// ```text
/// claude-cli/<version> (<user_type>, <entrypoint>[, agent-sdk/<v>][, client-app/<app>][, workload/<w>])
/// ```
///
/// WARNING: The `claude-cli/` prefix is used for log filtering.  Do **not**
/// alter it without updating the corresponding logging rules.
#[must_use]
pub fn user_agent(env: &UserAgentEnv, version: &str) -> String {
    // USER_TYPE: raw value; "undefined" when unset (JS semantics).
    let user_type = env.user_type.as_deref().unwrap_or("undefined");

    // CLAUDE_CODE_ENTRYPOINT: default "cli".
    let entrypoint = env.entrypoint.as_deref().unwrap_or("cli");

    // Optional suffixes — each prefixed with ", " when present (http.ts:20,25,33).
    let agent_sdk = env
        .agent_sdk_version
        .as_deref()
        .map(|v| format!(", agent-sdk/{v}"))
        .unwrap_or_default();

    let client_app = env
        .client_app
        .as_deref()
        .map(|a| format!(", client-app/{a}"))
        .unwrap_or_default();

    let workload = env
        .workload
        .as_deref()
        .map(|w| format!(", workload/{w}"))
        .unwrap_or_default();

    format!("claude-cli/{version} ({user_type}, {entrypoint}{agent_sdk}{client_app}{workload})")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal case: only `USER_TYPE` and entrypoint set.
    /// Expected: `claude-cli/1.2.3 (external, cli)`
    #[test]
    fn user_agent_minimal() {
        let env = UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            agent_sdk_version: None,
            client_app: None,
            workload: None,
        };
        assert_eq!(
            user_agent(&env, "1.2.3"),
            "claude-cli/1.2.3 (external, cli)"
        );
    }

    /// All three optional suffixes present, in order: agent-sdk → client-app →
    /// workload.
    #[test]
    fn user_agent_with_all_suffixes() {
        let env = UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            agent_sdk_version: Some("0.9.0".to_string()),
            client_app: Some("my-app/1.0.0".to_string()),
            workload: Some("cron".to_string()),
        };
        assert_eq!(
            user_agent(&env, "2.0.0"),
            "claude-cli/2.0.0 (external, cli, agent-sdk/0.9.0, client-app/my-app/1.0.0, workload/cron)"
        );
    }

    /// When `USER_TYPE` env is unset the literal string `"undefined"` appears in
    /// the header (mirrors JS template-literal semantics).
    #[test]
    fn user_agent_unset_user_type_is_undefined() {
        let env = UserAgentEnv {
            user_type: None,
            entrypoint: Some("cli".to_string()),
            agent_sdk_version: None,
            client_app: None,
            workload: None,
        };
        let ua = user_agent(&env, "1.0.0");
        assert!(
            ua.contains("(undefined, cli)"),
            "expected '(undefined, cli)' in: {ua}"
        );
    }

    /// When `CLAUDE_CODE_ENTRYPOINT` is unset the default value `"cli"` is used.
    #[test]
    fn user_agent_default_entrypoint_cli() {
        let env = UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: None, // unset → default "cli"
            agent_sdk_version: None,
            client_app: None,
            workload: None,
        };
        let ua = user_agent(&env, "1.0.0");
        assert_eq!(
            ua, "claude-cli/1.0.0 (external, cli)",
            "default entrypoint must be 'cli'"
        );
    }
}
