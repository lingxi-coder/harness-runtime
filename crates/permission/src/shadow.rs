//! Shadowed-rule detection — a config-lint that warns when a *specific* allow
//! rule is unreachable because a *tool-wide* deny/ask rule for the same tool
//! always fires first.
//!
//! 1:1 port of claude-code `utils/permissions/shadowedRuleDetection.ts`
//! (`detectUnreachableRules`, `isAllowRuleShadowedByDenyRule`,
//! `isAllowRuleShadowedByAskRule`, `isSharedSettingSource`,
//! `generateFixSuggestion`). This is a pure detector consumed by a future
//! `/permissions` TUI screen / startup doctor warning — it is NOT on the
//! `authorize` enforcement path.
//!
//! Behavioral summary (matching TS):
//! - Only allow rules with *specific content* (`rule_content.is_some()`) can be
//!   shadowed — tool-wide allow rules are never reported.
//! - Deny shadowing is checked first (more severe: the tool is fully blocked);
//!   if an allow rule is deny-shadowed it is NOT also reported as ask-shadowed.
//! - Ask shadowing has one exception: for `Bash` with `sandbox_auto_allow`
//!   enabled, a tool-wide ask rule from a *personal* (non-shared) source does
//!   NOT shadow, because the user's own sandbox auto-allows sandboxed commands.
//!   Shared sources (project/policy/command) always warn, because other team
//!   members may not have sandbox enabled.

use crate::rule::{PermissionRule, PermissionRuleSource};

/// claude-code `BASH_TOOL_NAME` (`tools/BashTool/toolName.ts`).
const BASH_TOOL_NAME: &str = "Bash";

/// Type of shadowing that makes a rule unreachable.
///
/// 1:1 with TS `ShadowType = 'ask' | 'deny'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowType {
    /// Shadowed by a tool-wide ask rule (will always prompt).
    Ask,
    /// Shadowed by a tool-wide deny rule (completely blocked — more severe).
    Deny,
}

/// An unreachable permission rule with a human-readable explanation.
///
/// 1:1 with TS `UnreachableRule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreachableRule {
    /// The specific allow rule that can never be reached.
    pub rule: PermissionRule,
    /// Why it is unreachable (e.g. `Blocked by "Bash" deny rule (from …)`).
    pub reason: String,
    /// The tool-wide rule that shadows it.
    pub shadowed_by: PermissionRule,
    /// Whether the shadower is an ask or a deny rule.
    pub shadow_type: ShadowType,
    /// A suggested fix (remove the tool-wide rule, or the specific allow rule).
    pub fix: String,
}

/// Result of checking whether a single allow rule is shadowed.
///
/// 1:1 with TS `ShadowResult` discriminated union. The `shadowType` carried in
/// the TS union is implicit here: each checker only ever reports its own type
/// (deny-checker → `Deny`, ask-checker → `Ask`), so the caller tags the
/// `UnreachableRule` directly rather than re-reading it off the result.
enum ShadowResult<'a> {
    /// Not shadowed.
    NotShadowed,
    /// Shadowed by `shadowed_by` (an ask or deny tool-wide rule).
    Shadowed { shadowed_by: &'a PermissionRule },
}

/// Whether a permission-rule source is *shared* (visible to other users).
///
/// 1:1 with TS `isSharedSettingSource` (`:61-67`):
/// - `projectSettings`: committed to git, shared with team
/// - `policySettings`: enterprise-managed, pushed to all users
/// - `command`: from slash-command frontmatter, potentially shared
///
/// Personal sources (`userSettings`, `localSettings`, `cliArg`/`flagSettings`,
/// `session`) return `false`.
#[must_use]
pub fn is_shared_setting_source(source: PermissionRuleSource) -> bool {
    matches!(
        source,
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
            | PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed)
            | PermissionRuleSource::Command
    )
}

/// Format a rule source for display in warning messages.
///
/// 1:1 with TS `formatSource` → `permissionRuleSourceDisplayString` →
/// `getSettingSourceDisplayNameLowercase` (`settings/constants.ts:72`).
/// Strings are byte-locked to claude-code.
#[must_use]
pub(crate) fn format_source(source: PermissionRuleSource) -> &'static str {
    match source {
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User) => "user settings",
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project) => {
            "shared project settings"
        }
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local) => {
            "project local settings"
        }
        PermissionRuleSource::FlagSettings => "command line arguments",
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed) => {
            "enterprise managed settings"
        }
        PermissionRuleSource::CliArg => "CLI argument",
        PermissionRuleSource::Command => "command configuration",
        PermissionRuleSource::Session => "current session",
        // 2.1.215 `pgl` display map tail.
        PermissionRuleSource::ToolsNarrowing => "CLI tool narrowing",
        PermissionRuleSource::McpServerPolicy => "MCP server policy",
    }
}

/// Generate a fix suggestion based on the shadow type.
///
/// 1:1 with TS `generateFixSuggestion` (`:79-92`). The `tool_name` is taken
/// from the *shadowing* rule (matching TS `shadowingRule.ruleValue.toolName`).
#[must_use]
fn generate_fix_suggestion(
    shadow_type: ShadowType,
    shadowing_rule: &PermissionRule,
    shadowed_rule: &PermissionRule,
) -> String {
    let shadowing_source = format_source(shadowing_rule.source);
    let shadowed_source = format_source(shadowed_rule.source);
    let tool_name = &shadowing_rule.value.tool_name;

    match shadow_type {
        ShadowType::Deny => format!(
            "Remove the \"{tool_name}\" deny rule from {shadowing_source}, \
             or remove the specific allow rule from {shadowed_source}"
        ),
        ShadowType::Ask => format!(
            "Remove the \"{tool_name}\" ask rule from {shadowing_source}, \
             or remove the specific allow rule from {shadowed_source}"
        ),
    }
}

/// Check whether a specific allow rule is shadowed (completely blocked) by a
/// tool-wide deny rule.
///
/// 1:1 with TS `isAllowRuleShadowedByDenyRule` (`:160-184`). Only allow rules
/// with specific content are considered; the first tool-wide deny rule for the
/// same tool wins.
fn is_allow_rule_shadowed_by_deny_rule<'a>(
    allow_rule: &PermissionRule,
    deny_rules: &'a [PermissionRule],
) -> ShadowResult<'a> {
    // Only check allow rules that have specific content (e.g. "Bash(ls:*)").
    // Tool-wide allow rules conflict with tool-wide deny rules but are not
    // "shadowed".
    if allow_rule.value.rule_content.is_none() {
        return ShadowResult::NotShadowed;
    }
    let tool_name = &allow_rule.value.tool_name;

    // Find any tool-wide deny rule for the same tool.
    let shadowing = deny_rules
        .iter()
        .find(|deny| &deny.value.tool_name == tool_name && deny.value.rule_content.is_none());

    match shadowing {
        Some(deny) => ShadowResult::Shadowed { shadowed_by: deny },
        None => ShadowResult::NotShadowed,
    }
}

/// Check whether a specific allow rule is shadowed (will always prompt) by a
/// tool-wide ask rule.
///
/// 1:1 with TS `isAllowRuleShadowedByAskRule` (`:111-147`), including the
/// Bash + `sandbox_auto_allow` exception (`:139-144`): when shadowed by a
/// *personal*-source ask rule the user's own sandbox auto-allows, so it is NOT
/// reported; a shared-source ask rule always warns.
fn is_allow_rule_shadowed_by_ask_rule<'a>(
    allow_rule: &PermissionRule,
    ask_rules: &'a [PermissionRule],
    sandbox_auto_allow: bool,
) -> ShadowResult<'a> {
    // Only check allow rules that have specific content (e.g. "Bash(ls:*)").
    // Tool-wide allow rules cannot be shadowed by ask rules.
    if allow_rule.value.rule_content.is_none() {
        return ShadowResult::NotShadowed;
    }
    let tool_name = &allow_rule.value.tool_name;

    // Find any tool-wide ask rule for the same tool.
    let Some(shadowing) = ask_rules
        .iter()
        .find(|ask| &ask.value.tool_name == tool_name && ask.value.rule_content.is_none())
    else {
        return ShadowResult::NotShadowed;
    };

    // Special case: Bash with sandbox auto-allow from personal settings.
    // The sandbox exception is based on the ASK rule's source, not the allow
    // rule's source. If the ask rule is from personal settings, the user's own
    // sandbox will auto-allow. If the ask rule is from shared settings, other
    // team members may not have sandbox enabled.
    if tool_name == BASH_TOOL_NAME
        && sandbox_auto_allow
        && !is_shared_setting_source(shadowing.source)
    {
        return ShadowResult::NotShadowed;
    }
    // Fall through to mark as shadowed — shared settings should always warn.

    ShadowResult::Shadowed {
        shadowed_by: shadowing,
    }
}

/// Detect all unreachable (shadowed) allow rules.
///
/// 1:1 with TS `detectUnreachableRules` (`:193-234`). For each allow rule,
/// deny-shadowing is checked first (more severe); a deny-shadowed rule is NOT
/// also reported as ask-shadowed. `sandbox_auto_allow` mirrors TS
/// `options.sandboxAutoAllowEnabled`.
///
/// Note: TS pulls `allow`/`ask`/`deny` out of a `ToolPermissionContext` via
/// `getAllowRules`/`getAskRules`/`getDenyRules`; the Rust port takes the three
/// slices directly (Rust has no context object), which is the only structural
/// divergence — the per-rule logic is byte-faithful.
#[must_use]
pub fn detect_unreachable_rules(
    allow: &[PermissionRule],
    ask: &[PermissionRule],
    deny: &[PermissionRule],
    sandbox_auto_allow: bool,
) -> Vec<UnreachableRule> {
    let mut unreachable = Vec::new();

    for allow_rule in allow {
        // Check deny shadowing first (more severe).
        if let ShadowResult::Shadowed { shadowed_by } =
            is_allow_rule_shadowed_by_deny_rule(allow_rule, deny)
        {
            let shadow_source = format_source(shadowed_by.source);
            unreachable.push(UnreachableRule {
                rule: allow_rule.clone(),
                reason: format!(
                    "Blocked by \"{}\" deny rule (from {shadow_source})",
                    shadowed_by.value.tool_name
                ),
                fix: generate_fix_suggestion(ShadowType::Deny, shadowed_by, allow_rule),
                shadowed_by: shadowed_by.clone(),
                shadow_type: ShadowType::Deny,
            });
            // Don't also report ask-shadowing if deny-shadowed.
            continue;
        }

        // Check ask shadowing.
        if let ShadowResult::Shadowed { shadowed_by } =
            is_allow_rule_shadowed_by_ask_rule(allow_rule, ask, sandbox_auto_allow)
        {
            let shadow_source = format_source(shadowed_by.source);
            unreachable.push(UnreachableRule {
                rule: allow_rule.clone(),
                reason: format!(
                    "Shadowed by \"{}\" ask rule (from {shadow_source})",
                    shadowed_by.value.tool_name
                ),
                fix: generate_fix_suggestion(ShadowType::Ask, shadowed_by, allow_rule),
                shadowed_by: shadowed_by.clone(),
                shadow_type: ShadowType::Ask,
            });
        }
    }

    unreachable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::{PermissionBehavior, PermissionRuleValue};

    fn rule(
        tool: &str,
        content: Option<&str>,
        behavior: PermissionBehavior,
        source: PermissionRuleSource,
    ) -> PermissionRule {
        PermissionRule {
            value: PermissionRuleValue {
                tool_name: tool.to_string(),
                rule_content: content.map(str::to_string),
            },
            behavior,
            source,
        }
    }

    fn allow(tool: &str, content: Option<&str>, source: PermissionRuleSource) -> PermissionRule {
        rule(tool, content, PermissionBehavior::Allow, source)
    }
    fn ask(tool: &str, content: Option<&str>, source: PermissionRuleSource) -> PermissionRule {
        rule(tool, content, PermissionBehavior::Ask, source)
    }
    fn deny(tool: &str, content: Option<&str>, source: PermissionRuleSource) -> PermissionRule {
        rule(tool, content, PermissionBehavior::Deny, source)
    }

    // ── is_shared_setting_source ────────────────────────────────────────────

    #[test]
    fn shared_sources_are_project_policy_command() {
        assert!(is_shared_setting_source(PermissionRuleSource::Settings(
            lingxi_core::types::SettingsScope::Project
        )));
        assert!(is_shared_setting_source(PermissionRuleSource::Settings(
            lingxi_core::types::SettingsScope::Managed
        )));
        assert!(is_shared_setting_source(PermissionRuleSource::Command));
    }

    #[test]
    fn personal_sources_are_not_shared() {
        for s in [
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
            PermissionRuleSource::FlagSettings,
            PermissionRuleSource::CliArg,
            PermissionRuleSource::Session,
        ] {
            assert!(!is_shared_setting_source(s), "{s:?} should be personal");
        }
    }

    // ── deny shadowing ──────────────────────────────────────────────────────

    #[test]
    fn specific_allow_shadowed_by_tool_wide_deny() {
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let deny_rules = [deny(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &[], &deny_rules, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shadow_type, ShadowType::Deny);
        assert_eq!(
            out[0].reason,
            "Blocked by \"Bash\" deny rule (from shared project settings)"
        );
        assert_eq!(out[0].shadowed_by, deny_rules[0]);
        assert_eq!(out[0].rule, allow_rules[0]);
    }

    #[test]
    fn tool_wide_allow_is_never_reported() {
        // A bare "Bash" allow rule (rule_content == None) cannot be shadowed.
        let allow_rules = [allow(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let deny_rules = [deny(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let ask_rules = [ask(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &deny_rules, false);
        assert!(out.is_empty());
    }

    #[test]
    fn deny_reported_when_both_deny_and_ask_present() {
        // Deny is checked first and ask-shadowing is suppressed for that rule.
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let ask_rules = [ask(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let deny_rules = [deny(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &deny_rules, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shadow_type, ShadowType::Deny);
    }

    #[test]
    fn deny_only_matches_same_tool() {
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let deny_rules = [deny(
            "Edit",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &[], &deny_rules, false);
        assert!(out.is_empty());
    }

    #[test]
    fn specific_deny_does_not_shadow() {
        // A deny rule must be tool-wide (rule_content == None) to shadow.
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let deny_rules = [deny(
            "Bash",
            Some("rm:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &[], &deny_rules, false);
        assert!(out.is_empty());
    }

    // ── ask shadowing ───────────────────────────────────────────────────────

    #[test]
    fn specific_allow_shadowed_by_tool_wide_ask() {
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let ask_rules = [ask(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &[], false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shadow_type, ShadowType::Ask);
        assert_eq!(
            out[0].reason,
            "Shadowed by \"Bash\" ask rule (from shared project settings)"
        );
    }

    #[test]
    fn ask_shadow_for_non_bash_tool_ignores_sandbox() {
        // The sandbox exception is Bash-only; an Edit ask rule still shadows
        // even with sandbox_auto_allow + a personal source.
        let allow_rules = [allow(
            "Edit",
            Some("src/**"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let ask_rules = [ask(
            "Edit",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &[], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shadow_type, ShadowType::Ask);
    }

    // ── Bash + sandbox auto-allow exception (TS :139-144) ───────────────────

    #[test]
    fn bash_ask_from_personal_source_with_sandbox_not_shadowed() {
        // Bash ask from a PERSONAL source + sandbox_auto_allow → NOT shadowed.
        for personal in [
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
            PermissionRuleSource::FlagSettings,
            PermissionRuleSource::CliArg,
            PermissionRuleSource::Session,
        ] {
            let allow_rules = [allow(
                "Bash",
                Some("ls:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            )];
            let ask_rules = [ask("Bash", None, personal)];
            let out = detect_unreachable_rules(&allow_rules, &ask_rules, &[], true);
            assert!(
                out.is_empty(),
                "{personal:?} ask should not shadow under sandbox"
            );
        }
    }

    #[test]
    fn bash_ask_from_personal_source_without_sandbox_is_shadowed() {
        // Same personal ask, sandbox DISABLED → still shadowed.
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let ask_rules = [ask(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &[], false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shadow_type, ShadowType::Ask);
    }

    #[test]
    fn bash_ask_from_shared_source_with_sandbox_still_shadowed() {
        // Same ask from a SHARED (projectSettings) source + sandbox → still
        // shadowed (other team members may not have sandbox).
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
        )];
        let ask_rules = [ask(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &[], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shadow_type, ShadowType::Ask);
    }

    // ── fix-string byte-fidelity vs the TS template ─────────────────────────

    #[test]
    fn deny_fix_string_matches_ts_template() {
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
        )];
        let deny_rules = [deny(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &[], &deny_rules, false);
        assert_eq!(
            out[0].fix,
            "Remove the \"Bash\" deny rule from shared project settings, \
             or remove the specific allow rule from project local settings"
        );
    }

    #[test]
    fn ask_fix_string_matches_ts_template() {
        let allow_rules = [allow(
            "Bash",
            Some("ls:*"),
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
        )];
        let ask_rules = [ask(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )];
        let out = detect_unreachable_rules(&allow_rules, &ask_rules, &[], false);
        assert_eq!(
            out[0].fix,
            "Remove the \"Bash\" ask rule from shared project settings, \
             or remove the specific allow rule from project local settings"
        );
    }

    // ── multiple allow rules ────────────────────────────────────────────────

    #[test]
    fn each_shadowed_allow_rule_is_reported() {
        let allow_rules = [
            allow(
                "Bash",
                Some("ls:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                "Bash",
                Some("cat:*"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ),
            allow(
                "Edit",
                Some("src/**"),
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ), // not shadowed
        ];
        let deny_rules = [deny(
            "Bash",
            None,
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed),
        )];
        let out = detect_unreachable_rules(&allow_rules, &[], &deny_rules, false);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|u| u.shadow_type == ShadowType::Deny));
    }

    #[test]
    fn no_rules_no_unreachable() {
        assert!(detect_unreachable_rules(&[], &[], &[], false).is_empty());
        assert!(detect_unreachable_rules(&[], &[], &[], true).is_empty());
    }

    // ── display-string byte-fidelity for every source (vs TS) ───────────────

    #[test]
    fn format_source_matches_ts_display_strings() {
        assert_eq!(
            format_source(PermissionRuleSource::Settings(
                lingxi_core::types::SettingsScope::User
            )),
            "user settings"
        );
        assert_eq!(
            format_source(PermissionRuleSource::Settings(
                lingxi_core::types::SettingsScope::Project
            )),
            "shared project settings"
        );
        assert_eq!(
            format_source(PermissionRuleSource::Settings(
                lingxi_core::types::SettingsScope::Local
            )),
            "project local settings"
        );
        assert_eq!(
            format_source(PermissionRuleSource::FlagSettings),
            "command line arguments"
        );
        assert_eq!(
            format_source(PermissionRuleSource::Settings(
                lingxi_core::types::SettingsScope::Managed
            )),
            "enterprise managed settings"
        );
        assert_eq!(format_source(PermissionRuleSource::CliArg), "CLI argument");
        assert_eq!(
            format_source(PermissionRuleSource::Command),
            "command configuration"
        );
        assert_eq!(
            format_source(PermissionRuleSource::Session),
            "current session"
        );
        // 2.1.215 `pgl` tail sources.
        assert_eq!(
            format_source(PermissionRuleSource::ToolsNarrowing),
            "CLI tool narrowing"
        );
        assert_eq!(
            format_source(PermissionRuleSource::McpServerPolicy),
            "MCP server policy"
        );
    }

    #[test]
    fn new_sources_are_lowest_citation_priority_and_not_shared() {
        // Walk tail: session > toolsNarrowing > mcpServerPolicy (lowest).
        assert!(
            PermissionRuleSource::Session.priority()
                > PermissionRuleSource::ToolsNarrowing.priority()
        );
        assert!(
            PermissionRuleSource::ToolsNarrowing.priority()
                > PermissionRuleSource::McpServerPolicy.priority()
        );
        // userSettings still highest (walk head wins citation).
        assert!(
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User).priority()
                > PermissionRuleSource::Session.priority()
        );
        assert!(!is_shared_setting_source(
            PermissionRuleSource::ToolsNarrowing
        ));
        assert!(!is_shared_setting_source(
            PermissionRuleSource::McpServerPolicy
        ));
        assert_eq!(
            PermissionRuleSource::ToolsNarrowing.lingxi_settings_source(),
            "toolsNarrowing"
        );
        assert_eq!(
            PermissionRuleSource::McpServerPolicy.lingxi_settings_source(),
            "mcpServerPolicy"
        );
    }
}
