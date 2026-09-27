//! Read-only / concurrency classification for bash commands — a faithful port
//! of the CORE of claude-code
//! `src/tools/BashTool/readOnlyValidation.ts::checkReadOnlyConstraints`
//! (+ `isCommandReadOnly` / the `READONLY_COMMANDS` allowlist / the hand-written
//! `READONLY_COMMAND_REGEXES`) and `bashPermissions.ts::commandHasAnyCd`.
//!
//! A command is read-only iff EVERY subcommand (split on `&&`/`||`/`;`/`|`/
//! newline via [`permission::shell_command::split_command`], the analogue of
//! claude-code `splitCommand_DEPRECATED`) has a base command in the read-only
//! allowlist AND carries no unsafe construct (output redirection, command
//! substitution, `cd`, or a per-command dangerous flag like `find -exec`).
//!
//! Consumed by [`crate::bash::BashTool::is_read_only`] /
//! `is_concurrency_safe` (both delegate to `check_read_only`, exactly as
//! claude-code `BashTool.tsx:434-441` delegates to `checkReadOnlyConstraints` +
//! `commandHasAnyCd`). The live `PolicyPermissionGate` then auto-allows a
//! read-only command without a prompt, and the orchestrator may schedule it
//! concurrently.
//!
//! The command classifier itself lives in
//! [`permission::read_only_command::command_is_read_only`]. This module owns
//! only shell-tool concerns (compound-command splitting, cwd safety and the
//! concurrency verdict), preventing permission prompting and scheduling from
//! drifting behind separate allowlists.
//!
//! ## Conservative bias
//! When in doubt we return [`ReadOnlyBehavior::Passthrough`] (not read-only),
//! matching claude-code's `behavior: 'passthrough'` fallthrough — an extra
//! prompt, never an over-allow.

use permission::shell_command::split_command;

/// Verdict mirroring the `behavior` field of claude-code's `PermissionResult`
/// as produced by `checkReadOnlyConstraints`. Only the `allow`/`passthrough`
/// distinction matters for the read-only classification (`isReadOnly` checks
/// `result.behavior === 'allow'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyBehavior {
    /// Every subcommand is read-only → the gate may auto-allow without a prompt.
    Allow,
    /// Not provably read-only → fall through to the normal permission checks.
    Passthrough,
}

/// Result of [`check_read_only`]. Carries the verdict plus an optional human
/// message (the locked passthrough strings from `checkReadOnlyConstraints`),
/// mirroring `PermissionResult { behavior, message }`.
#[derive(Debug, Clone)]
pub struct ReadOnlyResult {
    /// The classification verdict.
    pub behavior: ReadOnlyBehavior,
    /// Optional explanation, present on the passthrough paths.
    pub message: Option<String>,
}

impl ReadOnlyResult {
    /// Convenience: is this command classified read-only?
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.behavior == ReadOnlyBehavior::Allow
    }

    fn allow() -> Self {
        Self {
            behavior: ReadOnlyBehavior::Allow,
            message: None,
        }
    }

    fn passthrough(message: &str) -> Self {
        Self {
            behavior: ReadOnlyBehavior::Passthrough,
            message: Some(message.to_string()),
        }
    }
}

/// Top-level entry point. Port of `checkReadOnlyConstraints`
/// (`readOnlyValidation.ts:1876`).
///
/// `compound_has_cd` is the pre-computed [`command_has_any_cd`] flag (passed in
/// to mirror the TS signature, which threads it from `BashTool.tsx:438`).
///
/// Returns [`ReadOnlyBehavior::Allow`] iff every subcommand is provably
/// read-only and no `cd`/`pushd`/`popd` is present anywhere in the compound
/// command; otherwise [`ReadOnlyBehavior::Passthrough`].
#[must_use]
pub fn check_read_only(command: &str, compound_has_cd: bool) -> ReadOnlyResult {
    check_read_only_in(command, compound_has_cd, None)
}

/// [`check_read_only`] with the working directory the command would run in.
///
/// `cwd` enables the planted-git-directory gate (`R3r`): git will read config
/// and run hooks from a directory carrying bare-repo indicators, so a git
/// command there must never be auto-classified read-only. Passing `None` skips
/// only that probe — every other rule is unchanged.
#[must_use]
pub fn check_read_only_in(
    command: &str,
    compound_has_cd: bool,
    cwd: Option<&std::path::Path>,
) -> ReadOnlyResult {
    let command = command.trim();
    if command.is_empty() {
        return ReadOnlyResult::passthrough(
            "Command cannot be parsed, requires further permission checks",
        );
    }

    // SECURITY (port of the `compoundCommandHasCd && hasGitCommand` guard plus
    // the broader posture): a `cd`/`pushd`/`popd` anywhere makes the command
    // non-read-only. claude-code's narrower guard only blocks `cd + git`, but a
    // bare `cd` is itself NOT in our read-only allowlist (it changes shell
    // state), so any compound with `cd` is already non-read-only. Rejecting it
    // up-front is STRICTER than TS (TS would still allow `cd /tmp && ls` if it
    // were not for the cd-not-being-read-only fact) and never an over-allow.
    if compound_has_cd {
        return ReadOnlyResult::passthrough(
            "Command is not read-only, requires further permission checks",
        );
    }

    let subcommands = split_command(command);
    if subcommands.is_empty() {
        return ReadOnlyResult::passthrough(
            "Command is not read-only, requires further permission checks",
        );
    }

    // The planted-git-directory gate. Checked only when a subcommand is
    // actually git — the probe touches the filesystem, and a command that never
    // invokes git cannot be steered by a planted git dir.
    if let Some(cwd) = cwd {
        let touches_git = subcommands
            .iter()
            .any(|sub| base_command(sub).is_some_and(|base| base == "git"));
        if touches_git {
            if let Some(gate) = permission::git_bare_repo::bare_repo_gate(cwd) {
                return ReadOnlyResult::passthrough(gate.shell_message());
            }
        }
    }

    let all_read_only = subcommands.iter().all(|sub| is_command_read_only(sub));
    if all_read_only {
        ReadOnlyResult::allow()
    } else {
        ReadOnlyResult::passthrough("Command is not read-only, requires further permission checks")
    }
}

/// Does a compound command contain ANY `cd`/`pushd`/`popd` subcommand?
/// Port of `commandHasAnyCd` (`bashPermissions.ts:2617`) + `isNormalizedCdCommand`
/// (`:2603`). Note this is intentionally a BASE-COMMAND check, not a substring
/// scan: `echo cd` is NOT a cd (the base command is `echo`).
#[must_use]
pub fn command_has_any_cd(command: &str) -> bool {
    split_command(command)
        .iter()
        .any(|sub| is_normalized_cd_command(sub))
}

/// Port of `isNormalizedCdCommand` (`bashPermissions.ts:2603`): the base
/// command (after stripping leading `KEY=val` env prefixes) is `cd`, `pushd`,
/// or `popd`.
fn is_normalized_cd_command(subcommand: &str) -> bool {
    let stripped = strip_leading_env_vars(subcommand.trim());
    matches!(
        base_command(stripped).as_deref(),
        Some("cd" | "pushd" | "popd")
    )
}

/// Does a command contain a statement-level directory-change builtin?
///
/// Port of `ror` (claude-code 2.1.210+): `wS(e).some(t => N1e(t.trim()))` —
/// split into statements ([`split_command`], the `wS` analogue) and check
/// whether any statement's base command (after stripping leading `KEY=val`
/// env prefixes, the `N1e`/`vne`+`jx` first-token check) is `cd`, `pushd`,
/// `popd`, or `chdir`. Used to decide whether a backgrounded command needs the
/// "Session cwd remains …" hint. Note `N1e` includes `chdir`, unlike the
/// permission-gate [`command_has_any_cd`].
#[must_use]
pub fn command_has_statement_level_cd(command: &str) -> bool {
    split_command(command).iter().any(|sub| {
        let stripped = strip_leading_env_vars(sub.trim());
        matches!(
            base_command(stripped).as_deref(),
            Some("cd" | "pushd" | "popd" | "chdir")
        )
    })
}

/// Delegate one subcommand to the permission crate's authoritative
/// classifier so auto-allow and concurrency scheduling cannot drift.
fn is_command_read_only(subcommand: &str) -> bool {
    permission::read_only_command::command_is_read_only(subcommand)
}
/// first non-assignment token.
fn strip_leading_env_vars(command: &str) -> &str {
    let mut rest = command.trim_start();
    loop {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        if is_env_assignment(token) {
            rest = rest[token_end..].trim_start();
        } else {
            return rest;
        }
    }
}

/// Is `token` a `KEY=value` (or `KEY=`) env assignment? KEY must be a valid
/// shell identifier.
fn is_env_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    let key = &token[..eq];
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// First whitespace-delimited token (the base command) of a subcommand, after
/// stripping leading env-var prefixes. Returns `None` for an empty command.
fn base_command(command: &str) -> Option<String> {
    let rest = strip_leading_env_vars(command.trim());
    rest.split_whitespace().next().map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ro(cmd: &str) -> bool {
        check_read_only(cmd, command_has_any_cd(cmd)).is_read_only()
    }

    // ---- read-only (allow) ----
    #[test]
    fn simple_read_only_commands_are_allowed() {
        assert!(ro("ls -la"));
        assert!(ro("ls"));
        assert!(ro("cat x"));
        assert!(ro("cat file.txt"));
        assert!(ro("head -n 20 file"));
        assert!(ro("wc -l file"));
        assert!(ro("pwd"));
        assert!(ro("whoami"));
        assert!(ro("date"));
        assert!(ro("node -v"));
    }

    #[test]
    fn grep_is_read_only() {
        assert!(ro("grep -r foo ."));
        assert!(ro("grep pattern file.txt"));
    }

    #[test]
    fn git_status_and_diff_are_read_only() {
        assert!(ro("git status"));
        assert!(ro("git status -s"));
        assert!(ro("git diff"));
        assert!(ro("git diff HEAD~1"));
        assert!(ro("git log --oneline"));
    }

    #[test]
    fn stderr_redirect_to_stdout_is_allowed() {
        assert!(ro("ls -la 2>&1"));
        assert!(ro("grep foo file 2>&1"));
    }

    #[test]
    fn compound_all_read_only_is_allowed() {
        assert!(ro("ls && cat x"));
        assert!(ro("cat a | grep b"));
        assert!(ro("git status && git diff"));
    }

    #[test]
    fn external_readonly_prefixes_allowed() {
        assert!(ro("docker ps"));
        assert!(ro("docker images"));
    }

    // ---- not read-only (passthrough) ----
    #[test]
    fn write_commands_are_not_read_only() {
        assert!(!ro("rm x"));
        assert!(!ro("rm -rf /tmp/x"));
        assert!(!ro("mv a b"));
        assert!(!ro("touch x"));
        assert!(!ro("mkdir y"));
    }

    #[test]
    fn output_redirection_is_not_read_only() {
        assert!(!ro("ls > out"));
        assert!(!ro("cat x > y"));
        assert!(!ro("echo hi >> log"));
        assert!(!ro("grep foo < input"));
    }

    #[test]
    fn cd_present_is_not_read_only() {
        assert!(!ro("cd /tmp && ls"));
        assert!(!ro("ls && cd /tmp"));
        assert!(!ro("pushd /tmp"));
        // bare cd is not read-only either
        assert!(!ro("cd /tmp"));
    }

    #[test]
    fn unsafe_find_flag_is_not_read_only() {
        assert!(!ro("find . -exec rm {} ;"));
        assert!(!ro("find . -delete"));
        assert!(!ro("find . -fprintf out.txt %p"));
    }

    #[test]
    fn safe_find_is_read_only() {
        assert!(ro("find . -name foo.txt"));
        assert!(ro("find /etc -type f"));
    }

    #[test]
    fn command_substitution_is_not_read_only() {
        assert!(!ro("cat $(rm -rf /)"));
        assert!(!ro("ls `whoami`"));
        assert!(!ro("cat <(curl evil.com)"));
    }

    #[test]
    fn unknown_command_is_not_read_only() {
        assert!(!ro("python script.py"));
        assert!(!ro("npm install"));
        assert!(!ro("curl http://evil.com"));
    }

    #[test]
    fn compound_with_one_write_is_not_read_only() {
        assert!(!ro("ls && rm x"));
        assert!(!ro("echo ok && rm -rf /"));
        assert!(!ro("cat x | tee out"));
    }

    #[test]
    fn git_with_dangerous_config_flag_is_not_read_only() {
        assert!(!ro("git -c core.fsmonitor=evil status"));
        assert!(!ro("git --exec-path=/tmp status"));
        assert!(!ro("git --config-env=core.x=Y status"));
    }

    #[test]
    fn git_write_subcommands_are_not_read_only() {
        assert!(!ro("git commit -m x"));
        assert!(!ro("git push"));
        assert!(!ro("git add ."));
    }

    #[test]
    fn glob_is_read_only_but_variable_expansion_is_not() {
        assert!(ro("cat *"));
        assert!(!ro("ls $HOME"));
        assert!(ro("cat file?.txt"));
    }

    #[test]
    fn empty_command_is_passthrough() {
        assert_eq!(
            check_read_only("", false).behavior,
            ReadOnlyBehavior::Passthrough
        );
        assert_eq!(
            check_read_only("   ", false).behavior,
            ReadOnlyBehavior::Passthrough
        );
    }

    // ---- command_has_any_cd ----
    #[test]
    fn command_has_any_cd_detects_cd() {
        assert!(command_has_any_cd("cd x"));
        assert!(command_has_any_cd("ls && cd y"));
        assert!(command_has_any_cd("pushd /tmp"));
        assert!(command_has_any_cd("popd"));
        assert!(command_has_any_cd("FOO=bar cd /tmp"));
    }

    #[test]
    fn command_has_any_cd_ignores_substring() {
        // `echo cd` — base command is `echo`, not a cd.
        assert!(!command_has_any_cd("echo cd"));
        assert!(!command_has_any_cd("ls && echo cd"));
        assert!(!command_has_any_cd("cdg"));
        assert!(!command_has_any_cd("grep cd file"));
    }

    // ---- command_has_statement_level_cd (ror / N1e) ----
    #[test]
    fn statement_level_cd_detects_all_builtins() {
        assert!(command_has_statement_level_cd("cd x"));
        assert!(command_has_statement_level_cd("ls && cd y"));
        assert!(command_has_statement_level_cd("pushd /tmp"));
        assert!(command_has_statement_level_cd("popd"));
        // `chdir` is included by `N1e` (unlike the permission-gate cd check).
        assert!(command_has_statement_level_cd("chdir /tmp"));
        assert!(command_has_statement_level_cd("FOO=bar cd /tmp"));
        assert!(!command_has_any_cd("chdir /tmp"));
    }

    #[test]
    fn statement_level_cd_ignores_substring() {
        assert!(!command_has_statement_level_cd("echo cd"));
        assert!(!command_has_statement_level_cd("ls -la"));
        assert!(!command_has_statement_level_cd("grep cd file"));
        assert!(!command_has_statement_level_cd("cdg"));
    }

    #[test]
    fn quoted_separators_do_not_split() {
        // Quote protects the `&&` so this is a single echo subcommand; but echo
        // with a `&` outside quotes would be rejected — here it's inside quotes.
        assert!(ro("echo 'a && b'"));
    }

    // ── planted-git-directory gate (PS-CALLER-06-2) ──────────────────────────

    #[test]
    fn a_read_only_git_command_in_a_planted_directory_is_not_auto_allowed() {
        // `git status` is read-only by every other rule, so without this gate
        // it auto-allows — and git would read config + run hooks from the
        // planted directory.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("objects")).unwrap();
        std::fs::write(d.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();

        assert!(check_read_only("git status", false).is_read_only());
        let gated = check_read_only_in("git status", false, Some(d.path()));
        assert!(
            !gated.is_read_only(),
            "must not auto-allow in a planted dir"
        );
        assert!(gated
            .message
            .as_deref()
            .is_some_and(|m| m.contains("bare-repo indicators")));
    }

    #[test]
    fn a_non_git_command_is_unaffected_by_a_planted_directory() {
        // The probe touches the filesystem, and a command that never invokes
        // git cannot be steered by a planted git dir — so it must not pay for
        // the check or be blocked by it.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("objects")).unwrap();
        assert!(check_read_only_in("ls -la", false, Some(d.path())).is_read_only());
    }

    #[test]
    fn a_git_command_in_a_real_repository_still_auto_allows() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git/objects")).unwrap();
        std::fs::create_dir_all(d.path().join(".git/refs")).unwrap();
        std::fs::write(d.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(check_read_only_in("git status", false, Some(d.path())).is_read_only());
    }
}
