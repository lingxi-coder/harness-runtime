//! Command wrapping helpers for cwd tracking + extglob disable + env vars.
//!
//! Reproduces claude-code's `bashProvider.ts:156-187` wrap idiom so the
//! `BashShell` tool can:
//!
//!   1. Disable extended globs (bash `extglob`, zsh `EXTENDED_GLOB`) so
//!      user-typed glob patterns behave consistently across shells.
//!   2. Run the user command.
//!   3. Tail-record the post-exec working directory to `<cwd_file>` so the
//!      next invocation can resume from the same directory.
//!
//! The cwd file path is shell-single-quoted to defeat path-injection from
//! filenames with apostrophes.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 30-minute default timeout matches claude-code's
/// `DEFAULT_TIMEOUT = 30 * 60 * 1000` in `Shell.ts:44`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `(name, value)` tuples for the env-vars claude-code injects into every
/// `Shell.execute` spawn. The runtime impl in [`super::runner`] applies
/// these on top of the caller-supplied env.
pub const ENV_LINGXI_MARKER: (&str, &str) = ("LINGXI", "1");
/// Marks every spawned process as running inside a claude-code CHILD session —
/// claude-code `Uot` sets `LINGXI_CHILD_SESSION:"1"` UNCONDITIONALLY (#7).
pub const ENV_LINGXI_CHILD_SESSION: (&str, &str) = ("LINGXI_CHILD_SESSION", "1");
/// Forces `git`'s editor to a no-op so interactive git commands cannot
/// block the shell.
pub const ENV_GIT_EDITOR: (&str, &str) = ("GIT_EDITOR", "true");
/// Name of the `SHELL` env var inherited from the user's environment.
pub const ENV_SHELL: &str = "SHELL";
/// Name of the session-id env var the IDE bridge / hooks consume.
pub const ENV_LINGXI_SESSION_ID: &str = "LINGXI_SESSION_ID";
/// Name of the agent-identity env var claude-code's `Uot` injects into child
/// shell spawns. The value (see [`ai_agent_value`]) mirrors claude-code's
/// `Mer("agent")`.
pub const ENV_AI_AGENT: &str = "AI_AGENT";

/// Value claude-code's `Uot` writes to `AI_AGENT` for child shell spawns.
///
/// claude-code's minified `Mer(e)` returns
/// `` `claude-code_${VERSION.replace(/\./g,"-")}_${e}` `` and the bash-provider
/// spawn site calls `Uot({…, source:"agent"})`, so `e === "agent"` is hardcoded
/// at the spawn — `AI_AGENT` is therefore set UNCONDITIONALLY (not gated on an
/// optional value).
///
/// R-V1: the version is claude-code's `VERSION` (the parity target), NOT LingXi's
/// `CARGO_PKG_VERSION`. claude-code v2.1.217 emits `claude-code_2-1-217_agent`;
/// using `CARGO_PKG_VERSION` (0.12.0) leaked `claude-code_0-12-0_agent` to every
/// child process / hook reading `AI_AGENT`. LingXi is a 1:1 copy, so it presents
/// the claude-code version it replicates.
#[must_use]
pub fn ai_agent_value() -> String {
    format!(
        "claude-code_{}_agent",
        platform_api::CLAUDE_CODE_VERSION.replace('.', "-")
    )
}

/// Does this spawn binary correspond to claude-code's **bash provider**
/// (`n === "bash"`), as opposed to the powershell provider?
///
/// claude-code sets `SHELL: n==="bash"?S:void 0` — i.e. it writes `SHELL` only
/// for the posix bash provider (whose resolved `shellPath` is `bash` *or*
/// `zsh`) and OMITS it entirely for the powershell provider (whose binary is
/// `pwsh`/`powershell.exe`). We reproduce that by treating any `bash`/`zsh`
/// spawn binary as the bash provider.
#[must_use]
pub fn is_bash_provider_shell(command: &str) -> bool {
    command.contains("bash") || command.contains("zsh")
}

/// Stable per-task output file path.
///
/// `<temp>/lingxi-task-output/<task_id>.out`. The directory is created on
/// first use by [`super::runner`].
#[must_use]
pub(crate) fn task_output_dir() -> PathBuf {
    platform_api::task_output::legacy_output_dir()
}

/// Stable path for one task output file.
#[must_use]
pub fn task_output_path(task_id: &str) -> PathBuf {
    platform_api::task_output::legacy_output_path(task_id)
}

/// Wrap a user command for the bash-tool spawn so we can track cwd
/// changes and disable extended globs.
///
/// Layout per claude-code's `bashProvider.ts:156-187`:
/// ```text
/// <extglob_disable> && <command> && pwd -P >| '<cwd_file>'
/// ```
///
/// `shell_path` is the absolute path of the spawn binary (`/bin/bash`,
/// `/bin/zsh`, …) so we pick the right idiom. When
/// `claude_code_shell_prefix_set` is true (caller's
/// `LINGXI_SHELL_PREFIX` env is non-empty), the combined bash+zsh
/// idiom is used because the wrapper may pick a different shell than
/// `shell_path`.
#[must_use]
pub fn wrap_command_for_cwd_tracking(
    command: &str,
    cwd_file: &Path,
    shell_path: &str,
    claude_code_shell_prefix_set: bool,
) -> String {
    let extglob = if claude_code_shell_prefix_set {
        Some("{ shopt -u extglob || setopt NO_EXTENDED_GLOB; } >/dev/null 2>&1 || true".to_string())
    } else if shell_path.contains("bash") {
        Some("shopt -u extglob 2>/dev/null || true".to_string())
    } else if shell_path.contains("zsh") {
        Some("setopt NO_EXTENDED_GLOB 2>/dev/null || true".to_string())
    } else {
        None
    };

    let escaped_cwd_file = shell_single_quote(&cwd_file.to_string_lossy());
    let pwd_tail = format!("pwd -P >| {escaped_cwd_file}");

    match extglob {
        Some(disable) => format!("{disable} && {command} && {pwd_tail}"),
        None => format!("{command} && {pwd_tail}"),
    }
}

/// Single-quote a string for bash. Replaces interior `'` with `'\''`.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_30_minutes() {
        assert_eq!(DEFAULT_TIMEOUT.as_secs(), 30 * 60);
    }

    #[test]
    fn env_constants_match_claude_code() {
        assert_eq!(ENV_LINGXI_MARKER, ("LINGXI", "1"));
        assert_eq!(ENV_LINGXI_CHILD_SESSION, ("LINGXI_CHILD_SESSION", "1"));
        assert_eq!(ENV_GIT_EDITOR, ("GIT_EDITOR", "true"));
        assert_eq!(ENV_SHELL, "SHELL");
        assert_eq!(ENV_LINGXI_SESSION_ID, "LINGXI_SESSION_ID");
        assert_eq!(ENV_AI_AGENT, "AI_AGENT");
    }

    #[test]
    fn ai_agent_value_matches_mer_agent_shape() {
        // claude-code `Mer("agent")` => `claude-code_<version-dashed>_agent`.
        let v = ai_agent_value();
        assert!(
            v.starts_with("claude-code_"),
            "AI_AGENT must start with the claude-code_ prefix: {v}"
        );
        assert!(
            v.ends_with("_agent"),
            "AI_AGENT must end with the _agent suffix: {v}"
        );
        // The version segment uses `-` separators, never `.` (Mer's replace).
        let mid = v
            .strip_prefix("claude-code_")
            .and_then(|s| s.strip_suffix("_agent"))
            .expect("prefix/suffix present");
        assert!(!mid.contains('.'), "version dots must be dashed: {v}");
        // R-V1: the version is the claude-code parity TARGET, NOT LingXi's
        // `CARGO_PKG_VERSION`. Asserted by derivation rather than against a
        // literal: a pinned `2-1-217` here made this test fail the moment the
        // target advanced — i.e. fail for being CORRECT. The version-specific
        // literals live in `test-harness/tests/parity_claude_2_1_<v>.rs`, which
        // is where a bump is supposed to be noticed.
        assert!(
            !v.contains(env!("CARGO_PKG_VERSION")),
            "must not leak LingXi's own version: {v}"
        );
        assert_eq!(
            v,
            format!(
                "claude-code_{}_agent",
                platform_api::CLAUDE_CODE_VERSION.replace('.', "-")
            )
        );
    }

    #[test]
    fn shell_gate_set_for_bash_provider_only() {
        // bash provider (`n==="bash"`): macOS resolves to /bin/zsh, Linux /bin/bash.
        assert!(is_bash_provider_shell("/bin/bash"));
        assert!(is_bash_provider_shell("/bin/zsh"));
        assert!(is_bash_provider_shell("/usr/local/bin/bash"));
        // powershell provider (`n==="powershell"`): SHELL is omitted (void 0).
        assert!(!is_bash_provider_shell("/usr/local/bin/pwsh"));
        assert!(!is_bash_provider_shell("powershell.exe"));
        // REPL interpreters are not the bash provider either.
        assert!(!is_bash_provider_shell("/usr/bin/python3"));
        assert!(!is_bash_provider_shell("node"));
    }

    #[test]
    fn shell_single_quote_handles_apostrophes() {
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn unknown_shell_omits_extglob_disable() {
        let wrapped =
            wrap_command_for_cwd_tracking("ls", Path::new("/tmp/cwd"), "/bin/fish", false);
        assert!(!wrapped.contains("shopt"), "fish must not include shopt");
        assert!(!wrapped.contains("setopt"), "fish must not include setopt");
        assert!(wrapped.contains("pwd -P >| '/tmp/cwd'"));
    }
}
