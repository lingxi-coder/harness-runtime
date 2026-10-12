//! `PermissionRule` — one allow/deny/ask entry, tagged with its source.
//!
//! Rules are evaluated in source-priority order: highest-priority source
//! wins, deny rules beat allow rules at the same priority.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// Name of the tool this rule applies to (e.g. `"Bash"`, `"Edit"`).
    pub tool_name: String,
    /// Optional rule-specific content used by tool-aware classifiers
    /// (e.g. a bash command pattern). `None` matches the bare tool.
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub rule_content: Option<Utf16JsonProjection>,
    /// Original stored permission spelling when parsing normalized it. This
    /// source-only metadata is excluded from serialization and semantic equality.
    #[serde(skip)]
    pub original_rule: Option<Utf16JsonProjection>,
}

impl PartialEq for PermissionRuleValue {
    fn eq(&self, other: &Self) -> bool {
        self.tool_name == other.tool_name && self.rule_content == other.rule_content
    }
}

impl Eq for PermissionRuleValue {}

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
        Self::from_rule_projection(&Utf16JsonProjection::from(rule))
            .expect("UTF-8 rule text has a representable tool name")
    }

    /// Parse a raw rule-string projection without narrowing content to UTF-8.
    /// A rule whose tool-name portion contains unpaired UTF-16 units is
    /// rejected because tool names are typed Rust identifiers in this API.
    #[must_use]
    pub fn from_rule_projection(rule: &Utf16JsonProjection) -> Option<Self> {
        let units = rule.string_units("")?;
        let bare = || {
            let tool_name = String::from_utf16(&units).ok()?;
            Some(Self {
                tool_name: normalize_legacy_tool_name(&tool_name),
                rule_content: None,
                original_rule: None,
            })
        };
        let mut value = if let Some(open) = find_first_unescaped_unit(&units, b'(' as u16) {
            let Some(close) = find_last_unescaped_unit(&units, b')' as u16) else {
                return bare();
            };
            // Malformed / content-after-close → treat the whole string as a tool name.
            if close <= open || close != units.len() - 1 {
                return bare();
            }
            let tool_name = String::from_utf16(&units[..open]).ok()?;
            if tool_name.is_empty() {
                return bare();
            }
            let raw_content = &units[open + 1..close];
            // `Tool()` / `Tool(*)` are tool-wide.
            if raw_content.is_empty() || raw_content == [b'*' as u16] {
                Self {
                    tool_name: normalize_legacy_tool_name(&tool_name),
                    rule_content: None,
                    original_rule: None,
                }
            } else {
                let content = unescape_rule_content_units(raw_content);
                Self {
                    tool_name: normalize_legacy_tool_name(&tool_name),
                    rule_content: Some(root_string_projection(content)),
                    original_rule: None,
                }
            }
        } else {
            bare()?
        };
        if value.to_rule_projection() != *rule {
            value.original_rule = Some(rule.clone());
        }
        Some(value)
    }

    /// Serialize back to the `"Tool"` / `"Tool(content)"` rule-string form
    /// (1:1 with claude-code `permissionRuleValueToString`; content parens are
    /// escaped). Round-trips with [`Self::from_rule_string`].
    #[must_use]
    pub fn to_rule_projection(&self) -> Utf16JsonProjection {
        let mut units: Vec<u16> = self.tool_name.encode_utf16().collect();
        if let Some(content) = &self.rule_content {
            if let Some(content_units) = content.string_units("") {
                units.push(b'(' as u16);
                units.extend(escape_rule_content_units(&content_units));
                units.push(b')' as u16);
            }
        }
        root_string_projection(units)
    }

    /// Stored spelling when Native's permission context must distinguish it
    /// from the canonical projection.
    #[must_use]
    pub fn original_rule_projection(&self) -> Option<&Utf16JsonProjection> {
        self.original_rule.as_ref()
    }

    /// UTF-8 display form for existing UI/log boundaries. Source serialization
    /// must use [`Self::to_rule_projection`] instead.
    #[must_use]
    pub fn to_rule_string(&self) -> String {
        self.to_rule_projection()
            .string_at("")
            .unwrap_or_default()
            .to_owned()
    }

    /// Exact content units, if this is a content rule.
    #[must_use]
    pub fn rule_content_units(&self) -> Option<Vec<u16>> {
        self.rule_content.as_ref()?.string_units("")
    }

    /// Exact UTF-8 view for string-only semantic helpers. Returns `None` for
    /// source strings containing unpaired UTF-16 units.
    #[must_use]
    pub fn rule_content_text(&self) -> Option<&str> {
        let content = self.rule_content.as_ref()?;
        let value = content.at_pointer("")?;
        crate::source_input::exact_string(value)
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
fn escape_rule_content_units(content: &[u16]) -> Vec<u16> {
    let mut escaped = Vec::with_capacity(content.len());
    for unit in content {
        if *unit == b'\\' as u16 || *unit == b'(' as u16 || *unit == b')' as u16 {
            escaped.push(b'\\' as u16);
        }
        escaped.push(*unit);
    }
    escaped
}

/// Reverse of [`escape_rule_content_units`] (claude-code `unescapeRuleContent`):
/// parens first, then backslashes.
fn unescape_rule_content_units(content: &[u16]) -> Vec<u16> {
    let mut units = content.to_vec();
    for escaped in [b'(' as u16, b')' as u16, b'\\' as u16] {
        let mut output = Vec::with_capacity(units.len());
        let mut index = 0;
        while index < units.len() {
            if units[index] == b'\\' as u16 && units.get(index + 1).copied() == Some(escaped) {
                output.push(escaped);
                index += 2;
            } else {
                output.push(units[index]);
                index += 1;
            }
        }
        units = output;
    }
    units
}

fn root_string_projection(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display is derived from exact UTF-16 units")
}

/// Index of the FIRST `target` char preceded by an EVEN number of backslashes
/// (i.e. not escaped). claude-code `findFirstUnescapedChar`.
fn find_first_unescaped_unit(units: &[u16], target: u16) -> Option<usize> {
    (0..units.len()).find(|&i| units[i] == target && is_unescaped_unit(units, i))
}

/// Index of the LAST unescaped `target` char. claude-code `findLastUnescapedChar`.
fn find_last_unescaped_unit(units: &[u16], target: u16) -> Option<usize> {
    (0..units.len())
        .rev()
        .find(|&i| units[i] == target && is_unescaped_unit(units, i))
}

/// `true` if the char at `i` is preceded by an even number of `\`.
fn is_unescaped_unit(units: &[u16], i: usize) -> bool {
    let mut backslashes = 0usize;
    let mut j = i;
    while j > 0 && units[j - 1] == b'\\' as u16 {
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
                original_rule: None,
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
                rule_content: Some("ls".into()),
                original_rule: None,
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
                original_rule: None,
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
        assert_eq!(v.rule_content_text(), Some("npm install"));
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
        assert_eq!(v.rule_content_text(), Some(r#"python -c "print(1)""#));
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
    fn parser_retains_noncanonical_stored_spelling_without_changing_rule_equality() {
        let stored = Utf16JsonProjection::from("KillBash");
        let parsed = PermissionRuleValue::from_rule_projection(&stored).unwrap();
        assert_eq!(parsed.to_rule_projection().string_at(""), Some("TaskStop"));
        assert_eq!(
            parsed
                .original_rule_projection()
                .and_then(|rule| rule.string_at("")),
            Some("KillBash")
        );

        let canonical = PermissionRuleValue {
            tool_name: "TaskStop".to_owned(),
            rule_content: None,
            original_rule: None,
        };
        assert_eq!(parsed, canonical);

        let already_canonical = PermissionRuleValue::from_rule_string("TaskStop");
        assert!(already_canonical.original_rule_projection().is_none());
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
                rule_content: None,
                original_rule: None,
            }
            .to_rule_string(),
            "Read"
        );
    }

    #[test]
    fn source_rule_content_keeps_isolated_utf16_units_through_parse_and_serialize() {
        let source = Utf16JsonProjection::parse(r#""Edit(\ud800)""#).unwrap();
        let rule = PermissionRuleValue::from_rule_projection(&source).unwrap();
        assert_eq!(rule.tool_name, "Edit");
        assert_eq!(rule.rule_content_units(), Some(vec![0xd800]));

        let serialized = rule.to_rule_projection();
        assert_eq!(
            serialized.string_units(""),
            Some(vec![
                b'E' as u16,
                b'd' as u16,
                b'i' as u16,
                b't' as u16,
                b'(' as u16,
                0xd800,
                b')' as u16,
            ])
        );
        assert_eq!(serialized.to_json_string().unwrap(), r#""Edit(\ud800)""#);

        let encoded = serde_json::to_string(&rule).unwrap();
        assert!(encoded.contains(r#""rule_content":"\ud800""#));
        let decoded: PermissionRuleValue = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, rule);
    }
}
