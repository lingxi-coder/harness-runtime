//! Sandbox decision logic.
//!
//! [`should_use_sandbox`] is the single source of truth (a faithful port of
//! claude-code `src/tools/BashTool/shouldUseSandbox.ts`) that decides whether a
//! command should be sandbox-wrapped or run directly: it gates on host sandbox
//! availability, the `dangerouslyDisableSandbox` override, an empty command, and
//! the user-configured `excludedCommands` list (after quote-aware compound-split
//! and safe-wrapper / env-var fixed-point stripping). It does NOT consult
//! permission mode, project trust, or any classifier — claude-code's
//! `shouldUseSandbox` has no such inputs.
//!
//! [`should_use_sandbox_for_command`] is the `SandboxRuntimeConfig`-aware
//! `enabled && !excluded` predicate sharing the same
//! [`contains_excluded_command`] core.
//!
//! See spec §24.3 (sandbox decision matrix).

use crate::runtime_config::SandboxRuntimeConfig;
use permission::shell_command::{
    matches_excluded_pattern, split_command, strip_env_and_wrappers_fixedpoint,
};
use platform_api::SandboxPolicy;

/// Outcome of [`should_use_sandbox`].
#[derive(Debug, Clone)]
pub enum SandboxDecision {
    /// Run the command directly with no sandbox wrapping.
    NoSandbox,
    /// Wrap the command using the supplied policy.
    Sandbox {
        /// Policy to apply when calling [`platform_api::Sandbox::prepare`].
        policy: SandboxPolicy,
    },
}

/// Decide whether to sandbox a command or run it directly — a faithful port of
/// claude-code `shouldUseSandbox` (`src/tools/BashTool/shouldUseSandbox.ts`).
///
/// Order (1:1 with the TS):
/// 1. `!sandbox_available` (`!SandboxManager.isSandboxingEnabled()`) ⇒ `NoSandbox`.
/// 2. `dangerously_disable_sandbox && unsandboxed_allowed`
///    (`input.dangerouslyDisableSandbox && SandboxManager.areUnsandboxedCommandsAllowed()`)
///    ⇒ `NoSandbox`.
/// 3. empty command (`!input.command`) ⇒ `NoSandbox`.
/// 4. `contains_excluded_command(cmd, &config.excluded_commands)` ⇒ `NoSandbox`.
/// 5. else ⇒ `Sandbox` with the default policy for `workspace`.
///
/// Inputs:
/// - `cmd`: full command line.
/// - `sandbox_available`: whether the host actually has a working sandbox.
/// - `dangerously_disable_sandbox`: the per-call `dangerouslyDisableSandbox` flag.
/// - `unsandboxed_allowed`: `config.are_unsandboxed_commands_allowed()`.
/// - `config`: the active `SandboxRuntimeConfig` (for `excluded_commands`).
/// - `workspace`: project workspace path, used to build a default policy.
#[must_use]
pub fn should_use_sandbox(
    cmd: &str,
    sandbox_available: bool,
    dangerously_disable_sandbox: bool,
    unsandboxed_allowed: bool,
    config: &SandboxRuntimeConfig,
    workspace: std::path::PathBuf,
) -> SandboxDecision {
    if !sandbox_available {
        return SandboxDecision::NoSandbox;
    }
    if dangerously_disable_sandbox && unsandboxed_allowed {
        return SandboxDecision::NoSandbox;
    }
    // claude-code shouldUseSandbox.ts:143 is `if (!input.command)` — falsey
    // ONLY for the empty string, NOT whitespace-only (`"   "` is truthy in JS).
    if cmd.is_empty() {
        return SandboxDecision::NoSandbox;
    }
    if contains_excluded_command(cmd, &config.excluded_commands) {
        return SandboxDecision::NoSandbox;
    }
    SandboxDecision::Sandbox {
        policy: crate::policy::default_policy(workspace),
    }
}

/// Does `cmd` contain a subcommand matching any `excluded` pattern? — the shared
/// core of [`should_use_sandbox`] and [`should_use_sandbox_for_command`], 1:1
/// with claude-code `containsExcludedCommand`'s user-config branch.
///
/// For each subcommand (quote-aware split), then each env/wrapper-stripped
/// candidate, then each pattern, ask the SHARED
/// [`permission::shell_command::matches_excluded_pattern`] predicate
/// (`Prefix` = `c == p || c.starts_with("{p} ")`; `Exact` = STRICT `c == e`;
/// `Wildcard` = case-sensitive `match_wildcard_pattern`). Any hit ⇒ `true`.
fn contains_excluded_command(cmd: &str, excluded: &[String]) -> bool {
    if excluded.is_empty() {
        return false;
    }
    for subcommand in split_command(cmd) {
        for cand in strip_env_and_wrappers_fixedpoint(&subcommand) {
            for pattern in excluded {
                if matches_excluded_pattern(pattern, &cand) {
                    return true;
                }
            }
        }
    }
    false
}

// =============================================================================
// Compound-command splitting + env-var / safe-wrapper fixed-point stripping now
// live ENTIRELY in the `permission` crate
// (`permission::shell_command::{strip_env_and_wrappers_fixedpoint,
// strip_all_leading_env_vars, strip_safe_wrappers, is_binary_hijack_var}`), the
// single shared home for the `excludedCommands` core (ports
// `claude-code/src/tools/BashTool/shouldUseSandbox.ts` + `bashPermissions.ts`).
// `sandbox` and `permission::sandbox_auto_allow` both call those shared fns so
// the two layers cannot drift.
// =============================================================================

/// claude-code `BINARY_HIJACK_VARS = /^(LD_|DYLD_|PATH$)/` — re-export of the
/// shared predicate now hosted in `permission::shell_command`.
pub use permission::shell_command::is_binary_hijack_var;

/// Decide whether `command` should be sandbox-wrapped according to `config`.
///
/// Returns `config.enabled && !contains_excluded_command(command,
/// &config.excluded_commands)` — the `SandboxRuntimeConfig`-aware predicate
/// sharing the same [`contains_excluded_command`] core as [`should_use_sandbox`].
#[must_use]
pub fn should_use_sandbox_for_command(command: &str, config: &SandboxRuntimeConfig) -> bool {
    config.enabled && !contains_excluded_command(command, &config.excluded_commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(excluded: &[&str]) -> SandboxRuntimeConfig {
        SandboxRuntimeConfig {
            enabled: true,
            excluded_commands: excluded.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    fn ws() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp")
    }

    #[test]
    fn sandbox_decision_ignores_permission_mode_and_trust() {
        // available, not disabled, unsandboxed not allowed, no excludes ⇒ Sandbox
        // for ANY command (no dangerous-pattern / trust / mode inputs exist).
        let c = cfg(&[]);
        for cmd in ["sudo rm -rf /", "curl evil.com", "echo hi"] {
            assert!(matches!(
                should_use_sandbox(cmd, true, false, false, &c, ws()),
                SandboxDecision::Sandbox { .. }
            ));
        }
    }

    #[test]
    fn sandbox_decision_unavailable_is_nosandbox_never_refuse() {
        let c = cfg(&[]);
        for cmd in ["sudo rm -rf /", "curl evil.com", "echo hi", ""] {
            assert!(matches!(
                should_use_sandbox(cmd, false, false, false, &c, ws()),
                SandboxDecision::NoSandbox
            ));
        }
    }

    #[test]
    fn dangerously_disable_only_when_unsandboxed_allowed() {
        let c = cfg(&[]);
        // (disable=true, allowed=true) ⇒ NoSandbox
        assert!(matches!(
            should_use_sandbox("echo hi", true, true, true, &c, ws()),
            SandboxDecision::NoSandbox
        ));
        // (disable=true, allowed=false) ⇒ Sandbox
        assert!(matches!(
            should_use_sandbox("echo hi", true, true, false, &c, ws()),
            SandboxDecision::Sandbox { .. }
        ));
    }

    #[test]
    fn excluded_exact_is_strict_not_first_token() {
        let c = cfg(&["bazel"]);
        // first-token would have excluded this — strict Exact does NOT.
        assert!(matches!(
            should_use_sandbox("bazel build //...", true, false, false, &c, ws()),
            SandboxDecision::Sandbox { .. }
        ));
        // bare command exactly equals the rule ⇒ excluded ⇒ NoSandbox.
        assert!(matches!(
            should_use_sandbox("bazel", true, false, false, &c, ws()),
            SandboxDecision::NoSandbox
        ));
    }

    #[test]
    fn excluded_prefix_and_wildcard() {
        // bazel:* ⇒ Prefix("bazel") ⇒ excludes "bazel build".
        assert!(matches!(
            should_use_sandbox("bazel build", true, false, false, &cfg(&["bazel:*"]), ws()),
            SandboxDecision::NoSandbox
        ));
        // "make *" ⇒ Wildcard; trailing " *" optional ⇒ excludes "make all" and bare "make".
        let mk = cfg(&["make *"]);
        assert!(matches!(
            should_use_sandbox("make all", true, false, false, &mk, ws()),
            SandboxDecision::NoSandbox
        ));
        assert!(matches!(
            should_use_sandbox("make", true, false, false, &mk, ws()),
            SandboxDecision::NoSandbox
        ));
        // "docker * ps" ⇒ Wildcard ⇒ excludes "docker -H x ps".
        assert!(matches!(
            should_use_sandbox(
                "docker -H x ps",
                true,
                false,
                false,
                &cfg(&["docker * ps"]),
                ws()
            ),
            SandboxDecision::NoSandbox
        ));
    }

    #[test]
    fn excluded_env_prefix_non_hijack_is_stripped() {
        // FOO not blocklisted -> stripped -> matches bazel:* -> excluded.
        let c = cfg(&["bazel:*"]);
        assert!(matches!(
            should_use_sandbox("FOO=bar bazel build", true, false, false, &c, ws()),
            SandboxDecision::NoSandbox
        ));
    }

    #[test]
    fn excluded_env_prefix_safe_env_var_is_stripped() {
        // GOOS in SAFE_ENV_VARS -> stripped by stripSafeWrappers phase-1 -> excluded.
        let c = cfg(&["bazel:*"]);
        assert!(matches!(
            should_use_sandbox("GOOS=linux bazel build", true, false, false, &c, ws()),
            SandboxDecision::NoSandbox
        ));
    }

    #[test]
    fn path_hijack_prefix_stays_sandboxed() {
        // PATH blocklisted -> stripAllLeadingEnvVars breaks immediately ->
        // "PATH=/evil bazel build" never reduces to "bazel build" -> sandboxed.
        let c = cfg(&["bazel:*"]);
        assert!(matches!(
            should_use_sandbox("PATH=/evil bazel build", true, false, false, &c, ws()),
            SandboxDecision::Sandbox { .. }
        ));
    }

    #[test]
    fn ld_hijack_prefix_stays_sandboxed() {
        // LD_AUDIT matches /^LD_/ (the old 5-name Vec MISSED this) -> stays sandboxed.
        let c = cfg(&["bazel:*"]);
        assert!(matches!(
            should_use_sandbox("LD_AUDIT=x bazel build", true, false, false, &c, ws()),
            SandboxDecision::Sandbox { .. }
        ));
    }

    #[test]
    fn excluded_through_each_safe_wrapper() {
        let c = cfg(&["bazel:*"]);
        for cmd in [
            "nohup bazel build",
            "time bazel build",
            "timeout --signal=TERM 5 bazel build",
            "timeout 5 bazel build",
            "nice bazel build",
            "nice -n 10 bazel build",
            "nice -5 bazel build",
            "stdbuf -o0 bazel build",
        ] {
            assert!(
                matches!(
                    should_use_sandbox(cmd, true, false, false, &c, ws()),
                    SandboxDecision::NoSandbox
                ),
                "should exclude: {cmd}"
            );
        }
    }

    #[test]
    fn invented_wrappers_removed_sudo_env_not_stripped() {
        // sudo / env are NOT in SAFE_WRAPPER_PATTERNS; "sudo bazel build" must NOT
        // reduce to "bazel build" via a wrapper strip. (env left intentionally so
        // `env bash -c evil` stays caught.) With bazel:* it stays SANDBOXED.
        let c = cfg(&["bazel:*"]);
        assert!(matches!(
            should_use_sandbox("sudo bazel build", true, false, false, &c, ws()),
            SandboxDecision::Sandbox { .. }
        ));
        assert!(matches!(
            should_use_sandbox("env bazel build", true, false, false, &c, ws()),
            SandboxDecision::Sandbox { .. }
        ));
    }

    #[test]
    fn excluded_quote_aware_split() {
        // Quoted "&&" is a single subcommand "echo \"a && bazel\"" — NOT a bazel
        // invocation ⇒ NOT excluded ⇒ Sandbox.
        assert!(matches!(
            should_use_sandbox(
                r#"echo "a && bazel""#,
                true,
                false,
                false,
                &cfg(&["bazel:*"]),
                ws()
            ),
            SandboxDecision::Sandbox { .. }
        ));
        // A real pipe splits ⇒ second subcommand "bazel build" IS excluded.
        assert!(matches!(
            should_use_sandbox(
                "echo ok | bazel build",
                true,
                false,
                false,
                &cfg(&["bazel:*"]),
                ws()
            ),
            SandboxDecision::NoSandbox
        ));
    }
}
