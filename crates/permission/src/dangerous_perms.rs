//! Dangerous-permission detection for auto mode.
//!
//! Ports the `isDangerous{Bash,PowerShell,Task}Permission` predicates,
//! `isDangerousClassifierPermission`, and `findDangerousClassifierPermissions`
//! from claude-code `utils/permissions/permissionSetup.ts:94-342`.
//!
//! An allow rule that lets the model run arbitrary code (e.g. `Bash(python:*)`,
//! `PowerShell(iex:*)`, `Agent(*)`) would bypass the auto-mode classifier's
//! safety evaluation. These predicates identify such rules so
//! [`crate::policy::PermissionPolicy::strip_dangerous_for_auto`] can strip them
//! on auto-mode entry and restore them on exit.
//!
//! ## Documented divergences (external build parity)
//! - The `USER_TYPE === 'ant'` `Tmux` special-case in
//!   `isDangerousClassifierPermission` (`permissionSetup.ts:276-279`) is
//!   OMITTED. TS only treats `Tmux` as dangerous for `ant` users; external
//!   builds (and this port) do not.
//! - The ant-only tail of `DANGEROUS_BASH_PATTERNS` is omitted in
//!   [`crate::dangerous_patterns`] (documented there).
//!
//! The exact-shape matcher itself is byte-faithful to TS.

use crate::dangerous_patterns::{
    dangerous_bash_patterns, CROSS_PLATFORM_CODE_EXEC, POWERSHELL_DANGEROUS_PATTERNS,
};
use crate::rule::{
    normalize_legacy_tool_name, PermissionBehavior, PermissionRule, PermissionRuleSource,
    PermissionRuleValue,
};

/// Canonical tool names (claude-code `tools/{BashTool,PowerShellTool}/toolName.ts`,
/// `tools/AgentTool/constants.ts`). Kept as local literals so the predicates
/// don't pull a dependency on the tool crates.
const BASH_TOOL_NAME: &str = "Bash";
const POWERSHELL_TOOL_NAME: &str = "PowerShell";
const AGENT_TOOL_NAME: &str = "Agent";
/// The Android mobile shell tool (`tools/shell-mobile` `TOOL_NAME`). It runs
/// mksh/sh-compatible commands through the in-engine sandbox, so it shares
/// Bash's command-pattern rule semantics and dangerous-rule analysis.
const SHELL_TOOL_NAME: &str = "Shell";

/// The exact-shape match used by both shell predicates against a single
/// lowercase pattern. 1:1 with the per-pattern body of
/// `isDangerousBashPermission` (`permissionSetup.ts:117-143`):
///
/// - `content == pattern`
/// - `content == "{pattern}:*"`
/// - `content == "{pattern}*"`
/// - `content == "{pattern} *"`
/// - `content.starts_with("{pattern} -") && content.ends_with('*')`
///
/// `content` and `pattern` are both already lowercased by the caller.
fn matches_pattern_shape(content: &str, pattern: &str) -> bool {
    content == pattern
        || content == format!("{pattern}:*")
        || content == format!("{pattern}*")
        || content == format!("{pattern} *")
        || (content.starts_with(&format!("{pattern} -")) && content.ends_with('*'))
}

/// Checks if a Bash permission rule is dangerous for auto mode. 1:1 with
/// `isDangerousBashPermission` (`permissionSetup.ts:94-147`).
///
/// A rule is dangerous if it would auto-allow commands that execute arbitrary
/// code, bypassing the classifier's safety evaluation:
/// 1. Tool-level allow (`Bash` with no content, or `Bash(*)`) — allows ALL
///    commands. (`content` is `None` here because the parser collapses
///    `Bash()`/`Bash(*)` to `rule_content: None`.)
/// 2. Standalone wildcard (`*`).
/// 3. A dangerous interpreter pattern in any of the rule-shape variants.
#[must_use]
pub fn is_dangerous_bash_permission(tool_name: &str, rule_content: &Option<String>) -> bool {
    // Only check Bash rules (and the Android mobile `Shell` tool, which runs the
    // same mksh/sh-compatible command patterns — a dangerous `Shell(python:*)`
    // allow rule would bypass the auto-mode classifier just like `Bash(python:*)`).
    if tool_name != BASH_TOOL_NAME && tool_name != SHELL_TOOL_NAME {
        return false;
    }

    // Tool-level allow (Bash with no content, or Bash(*)) — allows ALL commands.
    let raw = match rule_content {
        None => return true,
        Some(c) if c.is_empty() => return true,
        Some(c) => c,
    };

    // Whitespace/star-only content (`Bash(**)`, `Bash( * )`) — 2.1.211 `Qqr`'s
    // caller `Xqr`'s `/^[\s*]+$/` test on the UNTRIMMED content.
    if !raw.is_empty() && raw.chars().all(|c| c == '*' || c.is_whitespace()) {
        return true;
    }

    let content = raw.trim().to_lowercase();

    // Standalone wildcard (*) matches everything.
    if content == "*" {
        return true;
    }

    // Check for dangerous patterns with prefix syntax (e.g. "python:*") or
    // wildcard syntax (e.g. "python*").
    for pattern in dangerous_bash_patterns() {
        let lower_pattern = pattern.to_lowercase();
        if matches_pattern_shape(&content, &lower_pattern) {
            // 2.1.211 `Qqr` python `-m <module>.<sub>` carve-out: a rule like
            // `Bash(python -m foo.bar:*)` is NOT dangerous (it can only reach the
            // `{pattern} -…*` arm, so testing the exemption after any match is
            // safe). The `_Tu`/`bTu` curl/wget/kubectl/aws/gcloud/gsutil
            // subcommand machinery is externally inert (`TTu` — the pattern list
            // — contains none of those bases) and is intentionally omitted.
            if is_python_dash_m_exempt(&lower_pattern, &content) {
                continue;
            }
            return true;
        }
    }

    false
}

/// 2.1.211 `Qqr` carve-out: `content` matched the `{pattern} -…*` dangerous arm,
/// but `pattern` is a `python`/`pythonN.N` interpreter AND the argument is a bare
/// `-m <module>.<submodule>` invocation → NOT dangerous. Mirrors
/// `/^python[\d.]*$/.test(o) && /^-m\s+\w+\.[\w.]+(\s*:|\s+)$/.test(s)` where
/// `s` is the arg with the trailing `*` stripped.
fn is_python_dash_m_exempt(pattern: &str, content: &str) -> bool {
    use std::sync::LazyLock;
    static PYTHON_BASE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^python[\d.]*$").unwrap());
    static DASH_M: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^-m\s+\w+\.[\w.]+(\s*:|\s+)$").unwrap());
    if !PYTHON_BASE.is_match(pattern) {
        return false;
    }
    // `i = content after "{pattern} "`, `s = i` with the trailing `*` removed.
    let Some(i) = content.strip_prefix(&format!("{pattern} ")) else {
        return false;
    };
    let s = match i.strip_suffix('*') {
        Some(s) => s,
        None => return false,
    };
    DASH_M.is_match(s)
}

/// Checks if a `PowerShell` permission rule is dangerous for auto mode. 1:1 with
/// `isDangerousPowerShellPermission` (`permissionSetup.ts:157-233`).
///
/// `PowerShell` is case-insensitive, so rule content is lowercased before
/// matching. In addition to the bash-style rule-shape variants, this checks the
/// `.exe`-on-first-word variant (`python` → `python.exe`, `npm run` →
/// `npm.exe run`) so a rule like `PowerShell(npm.exe run:*)` matches `npm run`.
#[must_use]
pub fn is_dangerous_powershell_permission(tool_name: &str, rule_content: &Option<String>) -> bool {
    if tool_name != POWERSHELL_TOOL_NAME {
        return false;
    }

    // Tool-level allow (PowerShell with no content, or PowerShell(*)).
    let raw = match rule_content {
        None => return true,
        Some(c) if c.is_empty() => return true,
        Some(c) => c,
    };

    // Whitespace/star-only content (`PowerShell(**)`, `PowerShell( * )`) —
    // 2.1.211 `Zqr`'s `/^[\s*]+$/` test, run on the UNTRIMMED content before
    // the trim/lowercase below.
    if !raw.is_empty() && raw.chars().all(|c| c == '*' || c.is_whitespace()) {
        return true;
    }

    let content = raw.trim().to_lowercase();

    // Standalone wildcard (*) matches everything.
    if content == "*" {
        return true;
    }

    // PS-specific cmdlet names. CROSS_PLATFORM_CODE_EXEC is shared with bash.
    let patterns = CROSS_PLATFORM_CODE_EXEC
        .iter()
        .chain(POWERSHELL_DANGEROUS_PATTERNS.iter());

    for pattern in patterns {
        // patterns stored lowercase; content lowercased above.
        if matches_pattern_shape(&content, pattern) {
            return true;
        }
        // .exe — goes on the FIRST word. `python` → `python.exe`.
        // `npm run` → `npm.exe run` (npm.exe is the real Windows binary name).
        // A rule like `PowerShell(npm.exe run:*)` needs to match `npm run`.
        let exe = match pattern.find(' ') {
            None => format!("{pattern}.exe"),
            Some(sp) => format!("{}.exe{}", &pattern[..sp], &pattern[sp..]),
        };
        if matches_pattern_shape(&content, &exe) {
            return true;
        }
    }
    false
}

/// Checks if an Agent (sub-agent) permission rule is dangerous for auto mode.
/// 1:1 with `isDangerousTaskPermission` (`permissionSetup.ts:240-245`).
///
/// Any Agent allow rule would auto-approve sub-agent spawns before the auto-mode
/// classifier can evaluate the sub-agent's prompt, defeating delegation-attack
/// prevention. The tool name is normalized (`Task` → `Agent`) first.
#[must_use]
pub fn is_dangerous_task_permission(tool_name: &str, _rule_content: &Option<String>) -> bool {
    normalize_legacy_tool_name(tool_name) == AGENT_TOOL_NAME
}

/// Checks if a permission rule is dangerous for auto mode (the OR of the three
/// shell/agent predicates). 1:1 with `tjr`/`isDangerousClassifierPermission`
/// (`permissionSetup.ts:272-285`), MINUS the `USER_TYPE === 'ant'` `Tmux` case
/// (see module doc).
///
/// This is the base (no-`classifyAllShell`) predicate. For the 2.1.211 auto-mode
/// escalation, callers that know the resolved `autoMode.classifyAllShell` setting
/// should use [`is_dangerous_classifier_permission_with_flag`] instead — this
/// wrapper is equivalent to passing `classify_all_shell = false`.
#[must_use]
pub fn is_dangerous_classifier_permission(tool_name: &str, rule_content: &Option<String>) -> bool {
    // The ant-only `Tmux` special-case (`permissionSetup.ts:276-279`) is omitted
    // for external parity.
    is_dangerous_bash_permission(tool_name, rule_content)
        || is_dangerous_powershell_permission(tool_name, rule_content)
        || is_dangerous_task_permission(tool_name, rule_content)
}

/// 2.1.211 `uxt` classifier-permission predicate WITH the `autoMode.classifyAllShell`
/// escalation. 1:1 with:
///
/// ```js
/// function uxt(e,t){if((e===$o||e===Si)&&ejr())return!0;return tjr(e,t)}
/// // $o = Bash tool, Si = PowerShell tool, ejr() = Jpi() = any settings source
/// //   has autoMode.classifyAllShell === true
/// ```
///
/// When `classify_all_shell` is set (the caller resolved
/// `autoMode.classifyAllShell === true` from any settings source via `Jpi`),
/// EVERY Bash/PowerShell allow rule is treated as dangerous so it is suspended
/// during auto mode and all shell commands route through the classifier — per
/// the settings schema: *"When true, every Bash/PowerShell allow rule is
/// suspended while auto mode is active so all shell commands are routed through
/// the classifier"*. Otherwise this is exactly the base
/// [`is_dangerous_classifier_permission`].
///
/// The Android mobile `Shell` tool (this port's Bash equivalent) is included in
/// the shell escalation, consistent with how [`is_dangerous_bash_permission`]
/// already treats it; this is an over-ask relative to the two upstream tool
/// names and is safe (a suspended allow rule only means the command is
/// re-evaluated by the classifier).
#[must_use]
pub fn is_dangerous_classifier_permission_with_flag(
    tool_name: &str,
    rule_content: &Option<String>,
    classify_all_shell: bool,
) -> bool {
    if classify_all_shell
        && (tool_name == BASH_TOOL_NAME
            || tool_name == POWERSHELL_TOOL_NAME
            || tool_name == SHELL_TOOL_NAME)
    {
        return true;
    }
    is_dangerous_classifier_permission(tool_name, rule_content)
}

/// Structured info about a dangerous permission found in the loaded rules.
/// 1:1 shape with the TS `DangerousPermissionInfo`
/// (`permissionSetup.ts:258-265`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerousPermissionInfo {
    /// The matched rule's value (tool name + optional content).
    pub rule_value: PermissionRuleValue,
    /// Where the rule came from.
    pub source: PermissionRuleSource,
    /// The rule formatted for display, e.g. `"Bash(*)"` or `"Bash(python:*)"`.
    pub rule_display: String,
    /// The source formatted for display, byte-exact with claude-code
    /// `formatPermissionSource` (`XUl`) via [`crate::shadow::format_source`]:
    /// e.g. `"user settings"`, `"shared project settings"`, `"CLI argument"`,
    /// `"enterprise managed settings"`. (WIZARD-06 S1 — the review UI shows this.)
    pub source_display: String,
}

/// Finds all dangerous ALLOW permissions among the loaded rules. 1:1 with the
/// settings-rules half of `findDangerousClassifierPermissions`
/// (`permissionSetup.ts:295-321`).
///
/// Divergence from TS: the TS function also takes a `cliAllowedTools: string[]`
/// and parses each `"Tool(content)"` spec (`:322-339`). In this port CLI
/// `--allowed-tools` are already parsed into [`PermissionRule`]s tagged with
/// [`PermissionRuleSource::CliArg`] by the loader, so they arrive through the
/// `rules` slice and the separate string-parsing pass is unnecessary.
#[must_use]
pub fn find_dangerous_classifier_permissions(
    rules: &[PermissionRule],
) -> Vec<DangerousPermissionInfo> {
    find_dangerous_classifier_permissions_with_flag(rules, false)
}

/// [`find_dangerous_classifier_permissions`] WITH the `autoMode.classifyAllShell`
/// escalation (`R1t`/`uxt`). When `classify_all_shell` is set, every `Bash`/
/// `PowerShell`/`Shell` allow rule is also reported as dangerous — 1:1 with the
/// TS `rjs` half of `findDangerousClassifierPermissions`, which walks each rule
/// through `R1t` (the flag-aware predicate) rather than the base `mon`. The base
/// [`find_dangerous_classifier_permissions`] is exactly this with the flag off.
#[must_use]
pub fn find_dangerous_classifier_permissions_with_flag(
    rules: &[PermissionRule],
    classify_all_shell: bool,
) -> Vec<DangerousPermissionInfo> {
    let mut dangerous = Vec::new();

    for rule in rules {
        if rule.behavior == PermissionBehavior::Allow
            && is_dangerous_classifier_permission_with_flag(
                &rule.value.tool_name,
                &rule.value.rule_content,
                classify_all_shell,
            )
        {
            // `Bash(python:*)` when content is set, else `Bash(*)` for tool-wide.
            let rule_string = match &rule.value.rule_content {
                Some(content) if !content.is_empty() => {
                    format!("{}({})", rule.value.tool_name, content)
                }
                _ => format!("{}(*)", rule.value.tool_name),
            };
            dangerous.push(DangerousPermissionInfo {
                rule_value: rule.value.clone(),
                source: rule.source,
                rule_display: rule_string,
                source_display: crate::shadow::format_source(rule.source).to_string(),
            });
        }
    }

    dangerous
}

#[cfg(test)]
mod tests {
    use super::*;

    // Always-`Some` by design: builds the `Some(content)` arg the predicates
    // take, mirroring a `Tool(content)` rule (vs `&None` for a tool-wide rule).
    #[allow(clippy::unnecessary_wraps)]
    fn content(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    // ── isDangerousBashPermission ──

    #[test]
    fn bash_interpreter_prefix_is_dangerous() {
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python:*")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("node:*")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("npm run:*")
        ));
    }

    #[test]
    fn bash_safe_command_is_not_dangerous() {
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("ls:*")
        ));
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("git status")
        ));
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("cat foo")
        ));
    }

    #[test]
    fn bash_tool_wide_is_dangerous() {
        // Bash with no content (`Bash`, `Bash()`, `Bash(*)` all parse to None).
        assert!(is_dangerous_bash_permission(BASH_TOOL_NAME, &None));
        assert!(is_dangerous_bash_permission(BASH_TOOL_NAME, &content("")));
    }

    #[test]
    fn bash_standalone_wildcard_is_dangerous() {
        assert!(is_dangerous_bash_permission(BASH_TOOL_NAME, &content("*")));
    }

    #[test]
    fn bash_shape_variants_match() {
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python*")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python *")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python -c*")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("PYTHON:*")
        )); // case-insensitive
    }

    #[test]
    fn bash_predicate_ignores_non_bash_tools() {
        assert!(!is_dangerous_bash_permission(
            "PowerShell",
            &content("python:*")
        ));
        assert!(!is_dangerous_bash_permission("Read", &None));
    }

    // ── mobile `Shell` tool shares Bash command semantics ──

    #[test]
    fn mobile_shell_dangerous_rules_are_flagged() {
        // The Android mobile `Shell` tool (`tools/shell-mobile` `TOOL_NAME`) runs
        // mksh/sh-compatible commands, so a dangerous `Shell` allow rule must be
        // flagged exactly like the same `Bash` rule — otherwise an auto-mode
        // classifier bypass is never stripped.
        assert!(is_dangerous_bash_permission(
            SHELL_TOOL_NAME,
            &content("python:*")
        ));
        assert!(is_dangerous_bash_permission(
            SHELL_TOOL_NAME,
            &content("node:*")
        ));
        // Tool-wide `Shell` allow lets the model run ALL commands.
        assert!(is_dangerous_bash_permission(SHELL_TOOL_NAME, &None));
        assert!(is_dangerous_bash_permission(SHELL_TOOL_NAME, &content("*")));
        // A safe `Shell` command root is not dangerous.
        assert!(!is_dangerous_bash_permission(
            SHELL_TOOL_NAME,
            &content("ls:*")
        ));
        // The OR classifier predicate also flags it.
        assert!(is_dangerous_classifier_permission(
            SHELL_TOOL_NAME,
            &content("python:*")
        ));
    }

    #[test]
    fn find_collects_dangerous_mobile_shell_allow_rules() {
        let rules = vec![
            allow(
                SHELL_TOOL_NAME,
                Some("python:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                SHELL_TOOL_NAME,
                Some("ls:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
        ];
        let found = find_dangerous_classifier_permissions(&rules);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule_display, "Shell(python:*)");
    }

    #[test]
    fn bash_whitespace_star_only_is_dangerous() {
        // 2.1.211 `Xqr`'s `/^[\s*]+$/` untrimmed check.
        for pat in ["**", " * ", "  ", "\t*"] {
            assert!(
                is_dangerous_bash_permission(BASH_TOOL_NAME, &content(pat)),
                "{pat:?} must be dangerous"
            );
        }
    }

    #[test]
    fn bash_python_dash_m_module_is_exempt() {
        // 2.1.211 `Qqr` carve-out: `python -m <module>.<sub>` is NOT dangerous,
        // but a bare `python …*` / other `python -flag*` still is.
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python -m foo.bar:*")
        ));
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python3 -m pkg.sub *")
        ));
        // Not the -m module shape → still dangerous.
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python -c*")
        ));
        assert!(is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("python:*")
        ));
    }

    #[test]
    fn bash_ant_only_tail_is_omitted() {
        // The ant-only tail (`gh`, `curl`, `git`, `aws`, …) is NOT dangerous in
        // the external build.
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("gh:*")
        ));
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("curl:*")
        ));
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("git:*")
        ));
        assert!(!is_dangerous_bash_permission(
            BASH_TOOL_NAME,
            &content("aws:*")
        ));
    }

    // ── isDangerousPowerShellPermission ──

    #[test]
    fn powershell_iex_is_dangerous() {
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("iex:*")
        ));
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("invoke-expression:*")
        ));
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("start-process:*")
        ));
    }

    #[test]
    fn powershell_exe_variant_is_dangerous() {
        // `python.exe` and `npm.exe run` variants must match.
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("python.exe:*")
        ));
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("npm.exe run:*")
        ));
    }

    #[test]
    fn powershell_tool_wide_and_wildcard_are_dangerous() {
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &None
        ));
        assert!(is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("*")
        ));
    }

    #[test]
    fn powershell_wmi_cim_are_dangerous() {
        // 2.1.211 Zqr WMI/CIM process-creation escape hatches.
        for pat in [
            "invoke-wmimethod:*",
            "iwmi:*",
            "invoke-cimmethod:*",
            "icim:*",
            "wmic:*",
            "wmic.exe:*", // .exe variant
        ] {
            assert!(
                is_dangerous_powershell_permission(POWERSHELL_TOOL_NAME, &content(pat)),
                "{pat} must be dangerous"
            );
        }
    }

    #[test]
    fn powershell_whitespace_star_only_is_dangerous() {
        // 2.1.211 Zqr `/^[\s*]+$/` on the untrimmed content.
        for pat in ["**", " * ", "  ", "\t*"] {
            assert!(
                is_dangerous_powershell_permission(POWERSHELL_TOOL_NAME, &content(pat)),
                "{pat:?} must be dangerous"
            );
        }
    }

    #[test]
    fn powershell_safe_command_is_not_dangerous() {
        assert!(!is_dangerous_powershell_permission(
            POWERSHELL_TOOL_NAME,
            &content("get-childitem:*")
        ));
    }

    #[test]
    fn powershell_predicate_ignores_non_ps_tools() {
        assert!(!is_dangerous_powershell_permission(
            "Bash",
            &content("iex:*")
        ));
    }

    // ── isDangerousTaskPermission ──

    #[test]
    fn agent_any_rule_is_dangerous() {
        assert!(is_dangerous_task_permission(AGENT_TOOL_NAME, &None));
        assert!(is_dangerous_task_permission(
            AGENT_TOOL_NAME,
            &content("general-purpose")
        ));
        // Legacy `Task` normalizes to `Agent`.
        assert!(is_dangerous_task_permission("Task", &content("anything")));
    }

    #[test]
    fn non_agent_task_permission_is_not_dangerous() {
        assert!(!is_dangerous_task_permission("Read", &None));
        assert!(!is_dangerous_task_permission("Bash", &content("ls:*")));
    }

    // ── isDangerousClassifierPermission (OR) ──

    #[test]
    fn classifier_predicate_is_or_of_three() {
        assert!(is_dangerous_classifier_permission(
            "Bash",
            &content("python:*")
        ));
        assert!(is_dangerous_classifier_permission(
            "PowerShell",
            &content("iex:*")
        ));
        assert!(is_dangerous_classifier_permission("Agent", &content("x")));
        assert!(!is_dangerous_classifier_permission("Read", &None));
        assert!(!is_dangerous_classifier_permission(
            "Bash",
            &content("ls:*")
        ));
    }

    // ── uxt: autoMode.classifyAllShell escalation ──

    #[test]
    fn classify_all_shell_flag_suspends_every_shell_allow() {
        // With classify_all_shell set, an otherwise-SAFE Bash/PowerShell allow
        // rule (e.g. `Bash(ls:*)`) becomes dangerous so it is suspended in auto
        // mode and the command routes through the classifier.
        assert!(is_dangerous_classifier_permission_with_flag(
            "Bash",
            &content("ls:*"),
            true
        ));
        assert!(is_dangerous_classifier_permission_with_flag(
            "PowerShell",
            &content("get-childitem:*"),
            true
        ));
        // The mobile `Shell` (Bash equivalent) is included in the escalation.
        assert!(is_dangerous_classifier_permission_with_flag(
            SHELL_TOOL_NAME,
            &content("ls:*"),
            true
        ));
    }

    #[test]
    fn classify_all_shell_flag_does_not_touch_non_shell_tools() {
        // The escalation only covers shell tools; a safe Read/Edit rule stays
        // non-dangerous even with the flag set (falls through to the base OR).
        assert!(!is_dangerous_classifier_permission_with_flag(
            "Read",
            &content("*"),
            true
        ));
        // An Agent rule is dangerous via the base predicate regardless of flag.
        assert!(is_dangerous_classifier_permission_with_flag(
            "Agent",
            &content("x"),
            true
        ));
    }

    #[test]
    fn classify_all_shell_flag_false_is_base_predicate() {
        // flag = false → exactly the base classifier: safe shell allow rules are
        // NOT dangerous.
        assert!(!is_dangerous_classifier_permission_with_flag(
            "Bash",
            &content("ls:*"),
            false
        ));
        assert!(!is_dangerous_classifier_permission_with_flag(
            "PowerShell",
            &content("get-childitem:*"),
            false
        ));
        // …but an inherently dangerous shell allow rule still is.
        assert!(is_dangerous_classifier_permission_with_flag(
            "Bash",
            &content("python:*"),
            false
        ));
    }

    // ── findDangerousClassifierPermissions ──

    fn allow(tool: &str, content: Option<&str>, source: PermissionRuleSource) -> PermissionRule {
        PermissionRule {
            value: PermissionRuleValue {
                tool_name: tool.to_string(),
                rule_content: content.map(str::to_string),
            },
            behavior: PermissionBehavior::Allow,
            source,
        }
    }

    #[test]
    fn find_collects_only_dangerous_allow_rules() {
        let rules = vec![
            allow(
                "Bash",
                Some("python:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                "Bash",
                Some("ls:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                "Read",
                None,
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow("Agent", None, PermissionRuleSource::CliArg),
            // a dangerous DENY rule must NOT be collected (only allow rules).
            PermissionRule {
                value: PermissionRuleValue {
                    tool_name: "Bash".into(),
                    rule_content: None,
                },
                behavior: PermissionBehavior::Deny,
                source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            },
        ];
        let found = find_dangerous_classifier_permissions(&rules);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].rule_display, "Bash(python:*)");
        assert_eq!(found[1].rule_display, "Agent(*)");
        // WIZARD-06 S1: source_display is byte-exact `formatPermissionSource`.
        assert_eq!(found[0].source_display, "user settings");
        assert_eq!(found[1].source_display, "CLI argument");
    }

    #[test]
    fn find_displays_tool_wide_as_star() {
        let rules = vec![allow(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let found = find_dangerous_classifier_permissions(&rules);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule_display, "Bash(*)");
    }

    #[test]
    fn find_with_flag_escalates_safe_shell_allows() {
        let rules = vec![
            allow(
                "Bash",
                Some("ls:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                "PowerShell",
                Some("gci:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                "Read",
                None,
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
        ];
        // flag OFF: none of these safe shell allows is dangerous.
        assert!(find_dangerous_classifier_permissions_with_flag(&rules, false).is_empty());
        assert!(find_dangerous_classifier_permissions(&rules).is_empty());
        // flag ON: both shell allows are collected (Read stays out).
        let found = find_dangerous_classifier_permissions_with_flag(&rules, true);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].rule_display, "Bash(ls:*)");
        assert_eq!(found[1].rule_display, "PowerShell(gci:*)");
    }
}
