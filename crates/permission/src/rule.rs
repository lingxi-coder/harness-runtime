//! `PermissionRule` — one allow/deny/ask entry, tagged with its source.
//!
//! Rules are evaluated in source-priority order: highest-priority source
//! wins, deny rules beat allow rules at the same priority.

use serde::{Deserialize, Serialize};

/// A single permission rule: the tool it targets, the behavior to apply,
/// and the configuration source it came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRule {
    /// What the rule matches against.
    pub value: PermissionRuleValue,
    /// What the rule does on match.
    pub behavior: PermissionBehavior,
    /// Where the rule came from (drives priority).
    pub source: PermissionRuleSource,
}

/// The match key for a `PermissionRule`: a tool name plus optional
/// rule-specific content (for example, a bash command substring).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PermissionRuleValue {
    /// Name of the tool this rule applies to (e.g. `"Bash"`, `"Edit"`).
    pub tool_name: String,
    /// Optional rule-specific content used by tool-aware classifiers
    /// (e.g. a bash command pattern). `None` matches the bare tool.
    pub rule_content: Option<String>,
}

impl PermissionRuleValue {
    /// Parse a permission-rule string (`"Tool"` or `"Tool(content)"`) into a
    /// [`PermissionRuleValue`]. 1:1 with claude-code
    /// `permissionRuleValueFromString` (`utils/permissions/permissionRuleParser.ts`):
    ///
    /// - Splits on the FIRST unescaped `(` and requires the LAST unescaped `)`
    ///   to be the final char; otherwise the whole string is a bare tool name.
    /// - Empty (`"Tool()"`) or wildcard (`"Tool(*)"`) content collapses to a
    ///   tool-wide rule (`rule_content: None`).
    /// - Content is unescaped (`\(`→`(`, `\)`→`)`, `\\`→`\`).
    /// - The tool name is run through [`normalize_legacy_tool_name`].
    #[must_use]
    pub fn from_rule_string(rule: &str) -> Self {
        let chars: Vec<char> = rule.chars().collect();
        let bare = || Self {
            tool_name: normalize_legacy_tool_name(rule),
            rule_content: None,
        };
        let Some(open) = find_first_unescaped(&chars, '(') else {
            return bare();
        };
        let Some(close) = find_last_unescaped(&chars, ')') else {
            return bare();
        };
        // Malformed / content-after-close → treat the whole string as a tool name.
        if close <= open || close != chars.len() - 1 {
            return bare();
        }
        let tool_name: String = chars[..open].iter().collect();
        if tool_name.is_empty() {
            return bare();
        }
        let raw_content: String = chars[open + 1..close].iter().collect();
        // `Tool()` / `Tool(*)` are tool-wide.
        if raw_content.is_empty() || raw_content == "*" {
            return Self {
                tool_name: normalize_legacy_tool_name(&tool_name),
                rule_content: None,
            };
        }
        Self {
            tool_name: normalize_legacy_tool_name(&tool_name),
            rule_content: Some(unescape_rule_content(&raw_content)),
        }
    }

    /// Serialize back to the `"Tool"` / `"Tool(content)"` rule-string form
    /// (1:1 with claude-code `permissionRuleValueToString`; content parens are
    /// escaped). Round-trips with [`Self::from_rule_string`].
    #[must_use]
    pub fn to_rule_string(&self) -> String {
        match self.rule_content.as_deref() {
            None | Some("") => self.tool_name.clone(),
            Some(content) => format!("{}({})", self.tool_name, escape_rule_content(content)),
        }
    }
}

/// Map a legacy tool name to its canonical name — 1:1 with the 2.1.195 binary
/// `LEGACY_TOOL_NAME_ALIASES` (`aDr`), excluding the removed Agent alias
/// (`Brief` is present unconditionally in the binary; the prior "KAIROS-gated,
/// omitted" note was based on stale TS). Applied on parse so rules/hooks resolve
/// to the current name.
#[must_use]
pub fn normalize_legacy_tool_name(name: &str) -> String {
    match name {
        "KillShell" | "KillBash" => "TaskStop",
        "AgentOutputTool" | "BashOutputTool" | "AgentOutput" | "BashOutput" => "TaskOutput",
        "ListPeers" => "ListAgents",
        "Brief" => "SendUserMessage",
        "ListMcpResources" => "ListMcpResourcesTool",
        "ReadMcpResource" => "ReadMcpResourceTool",
        "ReadMcpResourceDir" => "ReadMcpResourceDirTool",
        other => other,
    }
    .to_string()
}

/// Escape rule content for `Tool(content)` storage (claude-code
/// `escapeRuleContent`). Order matters: backslashes first, then parens.
fn escape_rule_content(content: &str) -> String {
    content
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

/// Reverse of [`escape_rule_content`] (claude-code `unescapeRuleContent`):
/// parens first, then backslashes.
fn unescape_rule_content(content: &str) -> String {
    content
        .replace("\\(", "(")
        .replace("\\)", ")")
        .replace("\\\\", "\\")
}

/// Index of the FIRST `target` char preceded by an EVEN number of backslashes
/// (i.e. not escaped). claude-code `findFirstUnescapedChar`.
fn find_first_unescaped(chars: &[char], target: char) -> Option<usize> {
    (0..chars.len()).find(|&i| chars[i] == target && is_unescaped(chars, i))
}

/// Index of the LAST unescaped `target` char. claude-code `findLastUnescapedChar`.
fn find_last_unescaped(chars: &[char], target: char) -> Option<usize> {
    (0..chars.len())
        .rev()
        .find(|&i| chars[i] == target && is_unescaped(chars, i))
}

/// `true` if the char at `i` is preceded by an even number of `\`.
fn is_unescaped(chars: &[char], i: usize) -> bool {
    let mut backslashes = 0usize;
    let mut j = i;
    while j > 0 && chars[j - 1] == '\\' {
        backslashes += 1;
        j -= 1;
    }
    backslashes.is_multiple_of(2)
}

/// What a rule does on match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionBehavior {
    /// Allow the call without prompting.
    Allow,
    /// Deny the call.
    Deny,
    /// Ask the user.
    Ask,
}

/// The configuration source a `PermissionRule` came from.
///
/// This type carries identity only. Do NOT read a precedence off the variant
/// order: see [`PermissionRuleSource::priority`], which encodes the CITATION
/// walk (`userSettings` highest) and is the exact reverse of the order in which
/// these same tiers win a settings VALUE merge. Both are correct, for different
/// questions, which is why neither is a property of the type.
///
/// (This doc comment previously said `Session` overrides everything and
/// `UserSettings` is lowest — the inverted reading that #35 fixed in
/// `priority` but left uncorrected here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionRuleSource {
    /// A settings tier — `~/.lingxi/settings.json`, the committed project file,
    /// the gitignored per-clone override, or managed policy.
    Settings(lingxi_core::types::SettingsScope),
    /// Rules attached to a feature flag.
    FlagSettings,
    /// Rules supplied on the command line.
    CliArg,
    /// Rules emitted by a command (e.g. `/permissions add`).
    Command,
    /// Session-scoped rules.
    Session,
    /// CLI `--tools` narrowing deny rules (claude-code `toolsNarrowing`). Walked
    /// AFTER `session` (lowest citation precedence). No producer yet — latent.
    ToolsNarrowing,
    /// Dynamic MCP server-policy rules (claude-code `mcpServerPolicy`). Walked
    /// last (lowest citation precedence). No producer yet — latent.
    McpServerPolicy,
}

impl PermissionRuleSource {
    /// Answers: which rule a denial CITES — NOT which rule wins.
    /// One of several orderings over these rungs; `lingxi_core::types::scope`'s module docs index them all and say which question each answers.
    ///
    /// Citation precedence matching claude-code's `Szn` walk
    /// (`userSettings` → `projectSettings` → `localSettings` → `flagSettings`
    /// → `policySettings` → `cliArg` → `command` → `session`), where the FIRST
    /// source in the walk wins citation. Encoded so that **higher index = higher
    /// precedence** (`userSettings` highest) — #35 (previously `session` was
    /// highest, the reverse of claude-code). NOTE: citation only; the deny-wins
    /// DECISION is behavior-first and independent of this rank.
    #[must_use]
    pub fn priority(self) -> u8 {
        // Comparison-only; absolute values are irrelevant, only the relative
        // order (= the `fJr` walk `[...S1, cliArg, command, session,
        // toolsNarrowing, mcpServerPolicy]` where the FIRST wins citation, so
        // userSettings highest ... mcpServerPolicy lowest).
        match self {
            Self::McpServerPolicy => 0,
            Self::ToolsNarrowing => 1,
            Self::Session => 2,
            Self::Command => 3,
            Self::CliArg => 4,
            Self::Settings(lingxi_core::types::SettingsScope::Managed) => 5,
            Self::FlagSettings => 6,
            Self::Settings(lingxi_core::types::SettingsScope::Local) => 7,
            Self::Settings(lingxi_core::types::SettingsScope::Project) => 8,
            Self::Settings(lingxi_core::types::SettingsScope::User) => 9,
        }
    }

    /// The RAW claude-code `SettingSource` identifier string for this source.
    ///
    /// This is the camelCase token claude-code interpolates verbatim into rule
    /// citations such as the `AgentTypeError` deny message
    /// `… from ${rule.source}.` — distinct from the human display strings in
    /// [`crate::shadow::format_source`] (`"user settings"` etc.). Byte-locked to
    /// claude-code's `SettingSource` union.
    #[must_use]
    pub fn lingxi_settings_source(self) -> &'static str {
        match self {
            Self::Settings(lingxi_core::types::SettingsScope::User) => "userSettings",
            Self::Settings(lingxi_core::types::SettingsScope::Project) => "projectSettings",
            Self::Settings(lingxi_core::types::SettingsScope::Local) => "localSettings",
            Self::FlagSettings => "flagSettings",
            Self::Settings(lingxi_core::types::SettingsScope::Managed) => "policySettings",
            Self::CliArg => "cliArg",
            Self::Command => "command",
            Self::Session => "session",
            Self::ToolsNarrowing => "toolsNarrowing",
            Self::McpServerPolicy => "mcpServerPolicy",
        }
    }
}

impl PermissionRule {
    /// (M6-05) Construct a session-scoped allow rule for the given tool.
    /// Used by `TuiPermissionGate` when the user picks `AllowAlways` in a
    /// permission dialog — the rule lives until the orchestrator session
    /// ends. M7 wires `/permissions` to surface and edit these rules.
    #[must_use]
    pub fn allow_tool_session(tool: &str) -> Self {
        Self {
            value: PermissionRuleValue {
                tool_name: tool.to_string(),
                rule_content: None,
            },
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Session,
        }
    }

    /// (M6-05) Whether this rule matches the bare tool name with an
    /// `Allow` behavior. Used by the session-rule lookup in
    /// `TuiPermissionGate::check` before the dialog opens.
    #[must_use]
    pub fn matches_tool(&self, tool: &str) -> bool {
        self.value.tool_name == tool
            && self.value.rule_content.is_none()
            && matches!(self.behavior, PermissionBehavior::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The remaining non-Agent tool aliases retain their existing semantics.
    #[test]
    fn non_agent_legacy_tool_name_aliases_match_binary() {
        for (legacy, canonical) in [
            ("KillShell", "TaskStop"),
            ("KillBash", "TaskStop"),
            ("AgentOutputTool", "TaskOutput"),
            ("BashOutputTool", "TaskOutput"),
            ("AgentOutput", "TaskOutput"),
            ("BashOutput", "TaskOutput"),
            ("ListPeers", "ListAgents"),
            ("Brief", "SendUserMessage"),
            ("ListMcpResources", "ListMcpResourcesTool"),
            ("ReadMcpResource", "ReadMcpResourceTool"),
            ("ReadMcpResourceDir", "ReadMcpResourceDirTool"),
        ] {
            assert_eq!(
                normalize_legacy_tool_name(legacy),
                canonical,
                "alias {legacy}"
            );
        }
        // Non-legacy names pass through unchanged.
        assert_eq!(normalize_legacy_tool_name("Bash"), "Bash");
        assert_eq!(normalize_legacy_tool_name("Agent"), "Agent");
    }

    #[test]
    fn user_settings_outrank_session() {
        // #35: claude-code's Szn cites userSettings before session, so
        // userSettings has the higher citation precedence (NOT session).
        assert!(
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User).priority()
                > PermissionRuleSource::Session.priority()
        );
    }

    /// Walks the whole `Szn` chain, not just one pair.
    ///
    /// #35 was a whole-chain inversion, and until now exactly one of its nine
    /// adjacent pairs was pinned (`user_settings_outrank_session` above) — the
    /// same regression could have landed again on any of the other eight with
    /// every test green.
    ///
    /// Written as a walk so a reordering fails at the FIRST pair that breaks
    /// and names it, rather than reporting a single opaque inequality.
    #[test]
    fn citation_precedence_walks_the_whole_szn_chain_descending() {
        use lingxi_core::types::SettingsScope as T;
        // claude-code `Szn`: userSettings → projectSettings → localSettings →
        // flagSettings → policySettings → cliArg → command → session, then the
        // two latent producers. FIRST in the walk wins citation, and `priority`
        // encodes that as the HIGHEST number.
        let chain = [
            PermissionRuleSource::Settings(T::User),
            PermissionRuleSource::Settings(T::Project),
            PermissionRuleSource::Settings(T::Local),
            PermissionRuleSource::FlagSettings,
            PermissionRuleSource::Settings(T::Managed),
            PermissionRuleSource::CliArg,
            PermissionRuleSource::Command,
            PermissionRuleSource::Session,
            PermissionRuleSource::ToolsNarrowing,
            PermissionRuleSource::McpServerPolicy,
        ];

        for pair in chain.windows(2) {
            let (before, after) = (pair[0], pair[1]);
            assert!(
                before.priority() > after.priority(),
                "{before:?} must cite before {after:?} ({} vs {})",
                before.priority(),
                after.priority()
            );
        }

        // Every rung distinct: a duplicate number would make two sources tie and
        // let the winner fall out of iteration order.
        let mut seen: Vec<u8> = chain.iter().map(|s| s.priority()).collect();
        seen.sort_unstable();
        let before_dedup = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before_dedup, "two sources share a priority");
    }

    /// Citation precedence and settings-value precedence are the two orderings
    /// that disagree, and both are right. Pinned together so that "fixing" one
    /// to match the other fails loudly instead of silently changing which rule
    /// a denial names.
    #[test]
    fn citation_precedence_is_the_reverse_of_value_precedence_for_the_file_tiers() {
        use lingxi_core::types::SettingsScope as T;
        // Citation: user cites before project cites before local.
        assert!(
            PermissionRuleSource::Settings(T::User).priority()
                > PermissionRuleSource::Settings(T::Project).priority()
        );
        assert!(
            PermissionRuleSource::Settings(T::Project).priority()
                > PermissionRuleSource::Settings(T::Local).priority()
        );
        // Value precedence runs the other way — `core::settings` folds
        // user, then project, then local, so local WINS a value conflict. That
        // is asserted by `env_beats_local_beats_project_beats_user_beats_defaults`
        // in `core/src/settings/mod.rs`; this comment is the link between the
        // two, since no single crate can see both.
    }

    // M6-05 Task 7: PermissionRule::allow_tool_session + matches_tool.

    #[test]
    fn allow_tool_session_marks_source_session() {
        let r = PermissionRule::allow_tool_session("Bash");
        assert_eq!(r.value.tool_name, "Bash");
        assert!(r.value.rule_content.is_none());
        assert!(matches!(r.behavior, PermissionBehavior::Allow));
        assert!(matches!(r.source, PermissionRuleSource::Session));
    }

    #[test]
    fn matches_tool_returns_true_for_same_tool_name() {
        let r = PermissionRule::allow_tool_session("Bash");
        assert!(r.matches_tool("Bash"));
        assert!(!r.matches_tool("Read"));
    }

    #[test]
    fn matches_tool_returns_false_when_rule_content_is_some() {
        let r = PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("ls".to_string()),
            },
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Session,
        };
        assert!(!r.matches_tool("Bash"));
    }

    #[test]
    fn matches_tool_returns_false_when_behavior_is_deny() {
        let r = PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: None,
            },
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Session,
        };
        assert!(!r.matches_tool("Bash"));
    }

    // ── rule-string parser (1:1 with claude-code permissionRuleParser.ts) ──

    fn parse(s: &str) -> PermissionRuleValue {
        PermissionRuleValue::from_rule_string(s)
    }

    #[test]
    fn parse_bare_tool_name() {
        let v = parse("Bash");
        assert_eq!(v.tool_name, "Bash");
        assert!(v.rule_content.is_none());
    }

    #[test]
    fn parse_tool_with_content() {
        let v = parse("Bash(npm install)");
        assert_eq!(v.tool_name, "Bash");
        assert_eq!(v.rule_content.as_deref(), Some("npm install"));
    }

    #[test]
    fn parse_empty_and_wildcard_collapse_to_tool_wide() {
        assert!(parse("Bash()").rule_content.is_none());
        assert!(parse("Bash(*)").rule_content.is_none());
        assert_eq!(parse("Bash(*)").tool_name, "Bash");
    }

    #[test]
    fn parse_unescapes_escaped_parens() {
        // claude-code doc example: Bash(python -c "print\(1\)")
        let v = parse(r#"Bash(python -c "print\(1\)")"#);
        assert_eq!(v.tool_name, "Bash");
        assert_eq!(v.rule_content.as_deref(), Some(r#"python -c "print(1)""#));
    }

    #[test]
    fn parse_malformed_treated_as_tool_name() {
        // No close paren, content after close, or empty tool name → bare name.
        assert_eq!(parse("Bash(npm").tool_name, "Bash(npm");
        assert!(parse("Bash(npm").rule_content.is_none());
        assert_eq!(parse("Bash(x)y").tool_name, "Bash(x)y");
        assert_eq!(parse("(foo)").tool_name, "(foo)");
    }

    #[test]
    fn parse_normalizes_legacy_tool_names() {
        assert_eq!(parse("Task").tool_name, "Task");
        assert_eq!(parse("Task(general-purpose)").tool_name, "Task");
        assert_eq!(parse("KillShell").tool_name, "TaskStop");
        assert_eq!(parse("AgentOutputTool").tool_name, "TaskOutput");
        assert_eq!(parse("BashOutputTool").tool_name, "TaskOutput");
    }

    #[test]
    fn to_rule_string_round_trips() {
        for s in [
            "Bash",
            "Bash(npm install)",
            "Edit(src/**)",
            "WebFetch(domain:example.com)",
        ] {
            assert_eq!(parse(s).to_rule_string(), s, "round-trip {s}");
        }
        // Parens in content are escaped on the way out and survive a re-parse.
        let v = parse(r#"Bash(echo "a\(b\)")"#);
        let s = v.to_rule_string();
        assert_eq!(s, r#"Bash(echo "a\(b\)")"#);
        assert_eq!(parse(&s).rule_content, v.rule_content);
        // None / empty content serialize to the bare tool name.
        assert_eq!(
            PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: None
            }
            .to_rule_string(),
            "Read"
        );
    }
}
